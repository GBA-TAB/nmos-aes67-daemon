//
//  nmos_is08.cpp
//
//  IS-08 (Audio Channel Mapping) REST API. Each Sink/Source exposes exactly
//  two Channel Mapping resources, matching the real ALSA-mediated audio path
//  in this daemon (see the extended comment on the IS-08 section of
//  nmos_manager.hpp for the full rationale):
//    Sink:   Input  = stream (RX) side, Output = ALSA side
//    Source: Input  = ALSA side,        Output = stream (TX) side
//  A crosspoint activation directly edits that Sink's/Source's own `map[]`
//  (map[stream_channel] = alsa_channel) — there is no cross-Sink-to-Source
//  resource. Deliberately reads SessionManager's own source/sink lists
//  (get_sources/get_sinks), not NmosManager's senders_/receivers_ IS-04
//  tracking maps: an activation calls session_manager_->add_sink()/
//  add_source(), which internally tears down and recreates the RTP stream
//  and fires SinkAdded/SourceAdded observer events processed asynchronously,
//  so senders_/receivers_ briefly lack the id being updated right after an
//  activation. SessionManager's own state has no such window.
//
//  Methods defined here are members of NmosManager (declared in
//  nmos_manager.hpp) — split into this file purely to keep nmos_manager.cpp
//  from growing further, matching this codebase's "one class, per-spec
//  implementation file" convention used for nmos_manager.cpp itself.
//

#include <sstream>

#include <boost/property_tree/json_parser.hpp>
#include <boost/property_tree/ptree.hpp>

#include "log.hpp"
#include "nmos_manager.hpp"

using NmosReq = NmosManager::NmosReq;
using NmosRes = NmosManager::NmosRes;

namespace {
void set_cm_headers(NmosRes& res) {
  res.set_header("Access-Control-Allow-Origin", "*");
  res.set_header("Access-Control-Allow-Methods", "GET, HEAD, POST, OPTIONS");
  res.set_header("Access-Control-Allow-Headers", "Content-Type, Accept");
  res.set_header("Cache-Control", "no-cache, no-store");
}

void cm_ok(NmosRes& res, const std::string& body) {
  set_cm_headers(res);
  res.set_content(body, "application/json");
}

void cm_not_found(NmosRes& res) {
  set_cm_headers(res);
  res.status = 404;
  res.set_content(R"({"code": 404, "error": "Not Found", "debug": ""})", "application/json");
}

void cm_bad_request(NmosRes& res, const std::string& msg) {
  set_cm_headers(res);
  res.status = 400;
  res.set_content("{\"code\": 400, \"error\": \"" + msg + "\", \"debug\": \"\"}",
                  "application/json");
}
}  // namespace

bool NmosManager::find_cm_input(const std::string& uuid, Is08Ref& ref) const {
  for (const auto& sink : session_manager_->get_sinks()) {
    if (is08_resource_id(Is08Kind::SinkStream, sink.id) == uuid) {
      ref = {Is08Kind::SinkStream, sink.id};
      return true;
    }
  }
  for (const auto& src : session_manager_->get_sources()) {
    if (is08_resource_id(Is08Kind::SourceAlsa, src.id) == uuid) {
      ref = {Is08Kind::SourceAlsa, src.id};
      return true;
    }
  }
  return false;
}

bool NmosManager::find_cm_output(const std::string& uuid, Is08Ref& ref) const {
  for (const auto& sink : session_manager_->get_sinks()) {
    if (is08_resource_id(Is08Kind::SinkAlsa, sink.id) == uuid) {
      ref = {Is08Kind::SinkAlsa, sink.id};
      return true;
    }
  }
  for (const auto& src : session_manager_->get_sources()) {
    if (is08_resource_id(Is08Kind::SourceStream, src.id) == uuid) {
      ref = {Is08Kind::SourceStream, src.id};
      return true;
    }
  }
  return false;
}

std::string NmosManager::is08_channels_json(size_t channel_count) const {
  std::ostringstream ss;
  ss << "[";
  for (size_t i = 0; i < channel_count; ++i) {
    if (i) ss << ", ";
    ss << "{\"label\": \"";
    if (channel_count == 2)
      ss << (i == 0 ? "Left" : "Right");
    else
      ss << "Ch" << (i + 1);
    ss << "\"}";
  }
  ss << "]";
  return ss.str();
}

std::string NmosManager::is08_map_active_json() const {
  std::ostringstream ss;
  ss << "{\"map\": {";
  bool first_output = true;

  // Sink ALSA-outputs: invert sink.map[] (stream_channel -> alsa_channel)
  // into alsa_channel -> stream_channel, since that's the only place this
  // daemon actually stores the association.
  for (const auto& sink0 : session_manager_->get_sinks()) {
    StreamSink sink;
    if (session_manager_->get_sink(sink0.id, sink)) continue;
    int32_t alsa_count = 0;
    session_manager_->get_alsa_input_count(alsa_count);
    if (alsa_count <= 0) continue;

    if (!first_output) ss << ", ";
    first_output = false;
    ss << "\"" << is08_resource_id(Is08Kind::SinkAlsa, sink.id) << "\": {";
    std::string stream_input_id = is08_resource_id(Is08Kind::SinkStream, sink.id);
    for (int32_t alsa_ch = 0; alsa_ch < alsa_count; ++alsa_ch) {
      if (alsa_ch) ss << ", ";
      ss << "\"" << alsa_ch << "\": ";
      int found_stream_ch = -1;
      for (size_t sch = 0; sch < sink.map.size(); ++sch) {
        if (sink.map[sch] == alsa_ch) {
          found_stream_ch = static_cast<int>(sch);
          break;
        }
      }
      if (found_stream_ch >= 0) {
        ss << "{\"input\": \"" << stream_input_id
           << "\", \"channel_index\": " << found_stream_ch << "}";
      } else {
        ss << "{\"input\": null, \"channel_index\": null}";
      }
    }
    ss << "}";
  }

  // Source stream-outputs: direct lookup, source.map[stream_ch] is always
  // some ALSA channel (no "unmapped" sentinel exists on this daemon's fixed
  // N-channel routing).
  for (const auto& src0 : session_manager_->get_sources()) {
    StreamSource src;
    if (session_manager_->get_source(src0.id, src)) continue;

    if (!first_output) ss << ", ";
    first_output = false;
    ss << "\"" << is08_resource_id(Is08Kind::SourceStream, src.id) << "\": {";
    std::string alsa_input_id = is08_resource_id(Is08Kind::SourceAlsa, src.id);
    for (size_t ch = 0; ch < src.map.size(); ++ch) {
      if (ch) ss << ", ";
      ss << "\"" << ch << "\": {\"input\": \"" << alsa_input_id
         << "\", \"channel_index\": " << static_cast<int>(src.map[ch]) << "}";
    }
    ss << "}";
  }

  ss << "}}";
  return ss.str();
}

void NmosManager::setup_is08_api() {
  nmos_get("/x-nmos/channelmapping/", [](const NmosReq&, NmosRes& res) {
    cm_ok(res, "[\"v1.0/\"]");
  });
  nmos_get("/x-nmos/channelmapping/v1.0/", [](const NmosReq&, NmosRes& res) {
    cm_ok(res, "[\"inputs/\", \"outputs/\", \"map/\"]");
  });
  nmos_get("/x-nmos/channelmapping/v1.0/map/", [](const NmosReq&, NmosRes& res) {
    cm_ok(res, "[\"active/\", \"activations/\"]");
  });

  // ---- Inputs: Sink stream-side + Source ALSA-side ----

  nmos_get("/x-nmos/channelmapping/v1.0/inputs/", [this](const NmosReq&, NmosRes& res) {
    std::ostringstream ss;
    ss << "[";
    bool first = true;
    for (const auto& sink : session_manager_->get_sinks()) {
      if (!first) ss << ", ";
      ss << "\"" << is08_resource_id(Is08Kind::SinkStream, sink.id) << "/\"";
      first = false;
    }
    for (const auto& src : session_manager_->get_sources()) {
      if (!first) ss << ", ";
      ss << "\"" << is08_resource_id(Is08Kind::SourceAlsa, src.id) << "/\"";
      first = false;
    }
    ss << "]";
    cm_ok(res, ss.str());
  });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/caps/?)",
          [this](const NmosReq& req, NmosRes& res) {
            Is08Ref ref;
            if (!find_cm_input(req.matches[1], ref)) { cm_not_found(res); return; }
            cm_ok(res, "{\"reordering\": false, \"block_size\": 1}");
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/parent/?)",
          [this](const NmosReq& req, NmosRes& res) {
            Is08Ref ref;
            if (!find_cm_input(req.matches[1], ref)) { cm_not_found(res); return; }
            if (ref.kind == Is08Kind::SinkStream) {
              cm_ok(res, "{\"id\": \"" + make_resource_uuid("receiver", ref.id) +
                            "\", \"type\": \"receiver\"}");
            } else {
              // Source's ALSA-side Input has no IS-04 parent — it's a local
              // hardware channel, not derived from any network resource.
              cm_ok(res, "{\"id\": null, \"type\": null}");
            }
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/channels/?)",
          [this](const NmosReq& req, NmosRes& res) {
            Is08Ref ref;
            if (!find_cm_input(req.matches[1], ref)) { cm_not_found(res); return; }
            if (ref.kind == Is08Kind::SinkStream) {
              StreamSink sink;
              if (session_manager_->get_sink(ref.id, sink)) { cm_not_found(res); return; }
              cm_ok(res, is08_channels_json(sink.map.size()));
            } else {
              int32_t count = 0;
              session_manager_->get_alsa_output_count(count);
              cm_ok(res, is08_channels_json(count > 0 ? static_cast<size_t>(count) : 0));
            }
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/properties/?)",
          [this](const NmosReq& req, NmosRes& res) {
            Is08Ref ref;
            if (!find_cm_input(req.matches[1], ref)) { cm_not_found(res); return; }
            if (ref.kind == Is08Kind::SinkStream) {
              StreamSink sink;
              if (session_manager_->get_sink(ref.id, sink)) { cm_not_found(res); return; }
              cm_ok(res, "{\"name\": \"" + sink.name + "\", \"description\": \"\"}");
            } else {
              StreamSource src;
              if (session_manager_->get_source(ref.id, src)) { cm_not_found(res); return; }
              cm_ok(res, "{\"name\": \"" + src.name + " (ALSA)\", \"description\": \"\"}");
            }
          });

  // ---- Outputs: Sink ALSA-side + Source stream-side ----

  nmos_get("/x-nmos/channelmapping/v1.0/outputs/", [this](const NmosReq&, NmosRes& res) {
    std::ostringstream ss;
    ss << "[";
    bool first = true;
    for (const auto& sink : session_manager_->get_sinks()) {
      if (!first) ss << ", ";
      ss << "\"" << is08_resource_id(Is08Kind::SinkAlsa, sink.id) << "/\"";
      first = false;
    }
    for (const auto& src : session_manager_->get_sources()) {
      if (!first) ss << ", ";
      ss << "\"" << is08_resource_id(Is08Kind::SourceStream, src.id) << "/\"";
      first = false;
    }
    ss << "]";
    cm_ok(res, ss.str());
  });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/caps/?)",
          [this](const NmosReq& req, NmosRes& res) {
            Is08Ref ref;
            if (!find_cm_output(req.matches[1], ref)) { cm_not_found(res); return; }
            // Scoped to the one valid pairing — a Sink's/Source's ALSA<->stream
            // mapping only ever makes sense against its own other side, never
            // another device's.
            std::string only_input = ref.kind == Is08Kind::SinkAlsa
                ? is08_resource_id(Is08Kind::SinkStream, ref.id)
                : is08_resource_id(Is08Kind::SourceAlsa, ref.id);
            cm_ok(res, "{\"routable_inputs\": [null, \"" + only_input + "\"]}");
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/sourceid/?)",
          [this](const NmosReq& req, NmosRes& res) {
            Is08Ref ref;
            if (!find_cm_output(req.matches[1], ref)) { cm_not_found(res); return; }
            if (ref.kind == Is08Kind::SourceStream) {
              cm_ok(res, "\"" + make_resource_uuid("source", ref.id) + "\"");
            } else {
              // Sink's ALSA-side Output isn't transmitted anywhere — no IS-04
              // Source is associated with it.
              cm_ok(res, "null");
            }
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/channels/?)",
          [this](const NmosReq& req, NmosRes& res) {
            Is08Ref ref;
            if (!find_cm_output(req.matches[1], ref)) { cm_not_found(res); return; }
            if (ref.kind == Is08Kind::SourceStream) {
              StreamSource src;
              if (session_manager_->get_source(ref.id, src)) { cm_not_found(res); return; }
              cm_ok(res, is08_channels_json(src.map.size()));
            } else {
              int32_t count = 0;
              session_manager_->get_alsa_input_count(count);
              cm_ok(res, is08_channels_json(count > 0 ? static_cast<size_t>(count) : 0));
            }
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/properties/?)",
          [this](const NmosReq& req, NmosRes& res) {
            Is08Ref ref;
            if (!find_cm_output(req.matches[1], ref)) { cm_not_found(res); return; }
            if (ref.kind == Is08Kind::SourceStream) {
              StreamSource src;
              if (session_manager_->get_source(ref.id, src)) { cm_not_found(res); return; }
              cm_ok(res, "{\"name\": \"" + src.name + "\", \"description\": \"\"}");
            } else {
              StreamSink sink;
              if (session_manager_->get_sink(ref.id, sink)) { cm_not_found(res); return; }
              cm_ok(res, "{\"name\": \"" + sink.name + " (ALSA)\", \"description\": \"\"}");
            }
          });

  // ---- Map ----

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/map/active/?)", [this](const NmosReq&, NmosRes& res) {
    cm_ok(res, is08_map_active_json());
  });

  nmos_post(R"(/x-nmos/channelmapping/v1\.0/map/activations/?)",
           [this](const NmosReq& req, NmosRes& res) {
             namespace pt_ns = boost::property_tree;
             pt_ns::ptree pt;
             try {
               std::istringstream ss(req.body);
               pt_ns::read_json(ss, pt);
             } catch (const std::exception& e) {
               cm_bad_request(res, e.what());
               return;
             }

             std::string mode =
                 pt.get_optional<std::string>("activation.mode").value_or("activate_immediate");
             if (mode != "activate_immediate") {
               res.status = 501;
               cm_bad_request(res, "Only activate_immediate is supported");
               return;
             }

             auto map_child = pt.get_child_optional("map");
             if (!map_child) {
               cm_bad_request(res, "missing map");
               return;
             }

             for (const auto& [output_uuid, channels] : *map_child) {
               Is08Ref out_ref;
               if (!find_cm_output(output_uuid, out_ref)) continue;

               for (const auto& [channel_str, entry] : channels) {
                 int outer_channel;
                 try {
                   outer_channel = std::stoi(channel_str);
                 } catch (...) {
                   continue;
                 }

                 // boost::property_tree's JSON parser has no null type — a
                 // JSON `null` comes back as the literal string "null" (same
                 // quirk patch_sender_staged/patch_receiver_staged already
                 // work around elsewhere in nmos_manager.cpp).
                 auto input_uuid = entry.get_optional<std::string>("input");
                 bool has_input = input_uuid && *input_uuid != "null" && !input_uuid->empty();
                 if (!has_input) {
                   // No physical "unmapped" sentinel exists on this daemon's
                   // fixed N-channel ALSA routing — clearing is a no-op.
                   continue;
                 }
                 int input_channel = entry.get_optional<int>("channel_index").value_or(0);

                 Is08Ref in_ref;
                 if (!find_cm_input(*input_uuid, in_ref)) continue;

                 if (out_ref.kind == Is08Kind::SinkAlsa) {
                   // Only this Sink's own stream-side Input may feed its
                   // ALSA-side Output.
                   if (in_ref.kind != Is08Kind::SinkStream || in_ref.id != out_ref.id) continue;

                   StreamSink sink;
                   if (session_manager_->get_sink(out_ref.id, sink)) continue;
                   if (input_channel < 0 ||
                       static_cast<size_t>(input_channel) >= sink.map.size())
                     continue;

                   sink.map[input_channel] = static_cast<uint8_t>(outer_channel);
                   session_manager_->add_sink(sink);
                 } else {
                   // Is08Kind::SourceStream — only this Source's own ALSA-
                   // side Input may feed its stream-side Output.
                   if (in_ref.kind != Is08Kind::SourceAlsa || in_ref.id != out_ref.id) continue;

                   StreamSource src;
                   if (session_manager_->get_source(out_ref.id, src)) continue;
                   if (outer_channel < 0 ||
                       static_cast<size_t>(outer_channel) >= src.map.size())
                     continue;

                   src.map[outer_channel] = static_cast<uint8_t>(input_channel);
                   session_manager_->add_source(src);
                 }
               }
             }

             cm_ok(res, is08_map_active_json());
           });
}
