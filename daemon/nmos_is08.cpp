//
//  nmos_is08.cpp
//
//  IS-08 (Audio Channel Mapping) REST API: one Input per Sink (Receiver),
//  one Output per Source (Sender). Reuses the existing nmos_get/nmos_post
//  route table (same port as IS-04/IS-05, no new server infra needed here
//  unlike IS-12).
//
//  Deliberately reads SessionManager's own source/sink lists (get_sources/
//  get_sinks) rather than NmosManager's senders_/receivers_ IS-04 tracking
//  maps: an activation calls session_manager_->add_source() to apply the
//  ALSA channel remap, which internally tears down and recreates the RTP
//  stream and fires SourceRemoved/SourceAdded observer events. Those events
//  are processed asynchronously (queued on events_mutex_/pending_events_),
//  so senders_ briefly does not contain the id being updated — reading from
//  it here raced with that window during testing (a request right after
//  activation would see the map as empty). SessionManager's own state has no
//  such window: it's updated synchronously inside add_source() before it
//  returns. The Source/Receiver IS-04 uuids needed for Output.SourceId and
//  Input.Parent are computed directly via make_resource_uuid — the same
//  deterministic function NmosManager itself uses to populate senders_/
//  receivers_ in the first place — so no dependency on that map is needed
//  here at all.
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

bool NmosManager::find_cm_input_sink_id(const std::string& uuid, uint8_t& sink_id) const {
  for (const auto& sink : session_manager_->get_sinks()) {
    if (is08_input_id(sink.id) == uuid) {
      sink_id = sink.id;
      return true;
    }
  }
  return false;
}

bool NmosManager::find_cm_output_source_id(const std::string& uuid, uint8_t& source_id) const {
  for (const auto& src : session_manager_->get_sources()) {
    if (is08_output_id(src.id) == uuid) {
      source_id = src.id;
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

  for (const auto& src : session_manager_->get_sources()) {
    if (!first_output) ss << ", ";
    first_output = false;
    ss << "\"" << is08_output_id(src.id) << "\": {";

    std::shared_lock lock(resources_mutex_);
    auto active_it = is08_active_map_.find(src.id);
    bool first_channel = true;
    for (size_t ch = 0; ch < src.map.size(); ++ch) {
      if (!first_channel) ss << ", ";
      first_channel = false;
      ss << "\"" << ch << "\": ";
      bool found = false;
      if (active_it != is08_active_map_.end()) {
        auto chan_it = active_it->second.find(static_cast<int>(ch));
        if (chan_it != active_it->second.end()) {
          ss << "{\"input\": \"" << chan_it->second.first
             << "\", \"channel_index\": " << chan_it->second.second << "}";
          found = true;
        }
      }
      if (!found) ss << "{\"input\": null, \"channel_index\": null}";
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

  // ---- Inputs (one per Sink) ----

  nmos_get("/x-nmos/channelmapping/v1.0/inputs/", [this](const NmosReq&, NmosRes& res) {
    std::ostringstream ss;
    ss << "[";
    bool first = true;
    for (const auto& sink : session_manager_->get_sinks()) {
      if (!first) ss << ", ";
      ss << "\"" << is08_input_id(sink.id) << "/\"";
      first = false;
    }
    ss << "]";
    cm_ok(res, ss.str());
  });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/caps/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (!find_cm_input_sink_id(req.matches[1], id)) { cm_not_found(res); return; }
            cm_ok(res, "{\"reordering\": false, \"block_size\": 1}");
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/parent/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (!find_cm_input_sink_id(req.matches[1], id)) { cm_not_found(res); return; }
            cm_ok(res, "{\"id\": \"" + make_resource_uuid("receiver", id) +
                          "\", \"type\": \"receiver\"}");
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/channels/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (!find_cm_input_sink_id(req.matches[1], id)) { cm_not_found(res); return; }
            StreamSink sink;
            if (session_manager_->get_sink(id, sink)) { cm_not_found(res); return; }
            cm_ok(res, is08_channels_json(sink.map.size()));
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/properties/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (!find_cm_input_sink_id(req.matches[1], id)) { cm_not_found(res); return; }
            StreamSink sink;
            if (session_manager_->get_sink(id, sink)) { cm_not_found(res); return; }
            cm_ok(res, "{\"name\": \"" + sink.name + "\", \"description\": \"\"}");
          });

  // ---- Outputs (one per Source) ----

  nmos_get("/x-nmos/channelmapping/v1.0/outputs/", [this](const NmosReq&, NmosRes& res) {
    std::ostringstream ss;
    ss << "[";
    bool first = true;
    for (const auto& src : session_manager_->get_sources()) {
      if (!first) ss << ", ";
      ss << "\"" << is08_output_id(src.id) << "/\"";
      first = false;
    }
    ss << "]";
    cm_ok(res, ss.str());
  });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/caps/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (!find_cm_output_source_id(req.matches[1], id)) { cm_not_found(res); return; }
            std::ostringstream ss;
            ss << "{\"routable_inputs\": [null";
            for (const auto& sink : session_manager_->get_sinks())
              ss << ", \"" << is08_input_id(sink.id) << "\"";
            ss << "]}";
            cm_ok(res, ss.str());
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/sourceid/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (!find_cm_output_source_id(req.matches[1], id)) { cm_not_found(res); return; }
            cm_ok(res, "\"" + make_resource_uuid("source", id) + "\"");
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/channels/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (!find_cm_output_source_id(req.matches[1], id)) { cm_not_found(res); return; }
            StreamSource src;
            if (session_manager_->get_source(id, src)) { cm_not_found(res); return; }
            cm_ok(res, is08_channels_json(src.map.size()));
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/properties/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (!find_cm_output_source_id(req.matches[1], id)) { cm_not_found(res); return; }
            StreamSource src;
            if (session_manager_->get_source(id, src)) { cm_not_found(res); return; }
            cm_ok(res, "{\"name\": \"" + src.name + "\", \"description\": \"\"}");
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
               uint8_t source_id;
               if (!find_cm_output_source_id(output_uuid, source_id)) continue;

               for (const auto& [channel_str, entry] : channels) {
                 int output_channel;
                 try {
                   output_channel = std::stoi(channel_str);
                 } catch (...) {
                   continue;
                 }

                 // boost::property_tree's JSON parser has no null type — a
                 // JSON `null` comes back as the literal string "null" (same
                 // quirk patch_sender_staged/patch_receiver_staged already
                 // work around for receiver_id/sender_id above).
                 auto input_uuid = entry.get_optional<std::string>("input");
                 bool has_input = input_uuid && *input_uuid != "null" && !input_uuid->empty();
                 int input_channel = entry.get_optional<int>("channel_index").value_or(0);

                 StreamSource src;
                 if (session_manager_->get_source(source_id, src)) continue;
                 if (output_channel < 0 ||
                     static_cast<size_t>(output_channel) >= src.map.size())
                   continue;

                 if (has_input) {
                   uint8_t sink_id;
                   if (!find_cm_input_sink_id(*input_uuid, sink_id)) continue;
                   StreamSink sink;
                   if (session_manager_->get_sink(sink_id, sink)) continue;
                   if (input_channel < 0 ||
                       static_cast<size_t>(input_channel) >= sink.map.size())
                     continue;

                   // The one genuinely novel design point: a Sink's captured
                   // channel X and a Source's playback channel X are the same
                   // physical ALSA channel, so activating this crosspoint is
                   // just copying the ALSA channel number across — this
                   // reuses the existing source-map-mutation path (the same
                   // one PUT /api/source/{id} already drives).
                   src.map[output_channel] = sink.map[input_channel];
                   session_manager_->add_source(src);

                   std::unique_lock lock(resources_mutex_);
                   is08_active_map_[source_id][output_channel] = {*input_uuid, input_channel};
                 } else {
                   // No physical "unmapped" sentinel exists on this daemon's
                   // fixed N-channel ALSA routing — clearing only updates
                   // the reported active map, the prior physical routing
                   // stays in place.
                   std::unique_lock lock(resources_mutex_);
                   auto it = is08_active_map_.find(source_id);
                   if (it != is08_active_map_.end()) it->second.erase(output_channel);
                 }
               }
             }

             cm_ok(res, is08_map_active_json());
           });
}
