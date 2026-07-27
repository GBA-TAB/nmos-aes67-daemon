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

#include <chrono>
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

void cm_locked(NmosRes& res, const std::string& msg) {
  set_cm_headers(res);
  res.status = 423;
  res.set_content("{\"code\": 423, \"error\": \"" + msg + "\", \"debug\": \"\"}",
                  "application/json");
}

void cm_no_content(NmosRes& res) {
  set_cm_headers(res);
  res.status = 204;
}

int64_t is08_now_ns() {
  return std::chrono::duration_cast<std::chrono::nanoseconds>(
             std::chrono::system_clock::now().time_since_epoch())
      .count();
}

// Mirrors nmos_manager.cpp's file-static make_version() (not reachable from
// this file) — same system_clock-epoch "<seconds>:<nanoseconds>" convention
// used for every other NMOS timestamp in this daemon, not real leap-second
// TAI (make_tai_timestamp() exists but is unused dead code elsewhere).
std::string ns_to_tai_str(int64_t ns) {
  int64_t secs = ns / 1'000'000'000LL;
  int64_t nsec = ns % 1'000'000'000LL;
  if (nsec < 0) {
    nsec += 1'000'000'000LL;
    secs -= 1;
  }
  return std::to_string(secs) + ":" + std::to_string(nsec);
}

bool parse_tai_str(const std::string& s, int64_t& ns_out) {
  auto colon = s.find(':');
  if (colon == std::string::npos) return false;
  try {
    int64_t sec = std::stoll(s.substr(0, colon));
    int64_t nsec = std::stoll(s.substr(colon + 1));
    ns_out = sec * 1'000'000'000LL + nsec;
    return true;
  } catch (...) {
    return false;
  }
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

bool NmosManager::find_alsa_input_channel(const std::string& uuid, uint8_t& channel) const {
  uint8_t count = config_->get_alsa_channels();
  for (uint8_t ch = 0; ch < count; ++ch) {
    if (is08_alsa_input_id(ch) == uuid) {
      channel = ch;
      return true;
    }
  }
  return false;
}

bool NmosManager::find_alsa_output_channel(const std::string& uuid, uint8_t& channel) const {
  uint8_t count = config_->get_alsa_channels();
  for (uint8_t ch = 0; ch < count; ++ch) {
    if (is08_alsa_output_id(ch) == uuid) {
      channel = ch;
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
  // This endpoint reports the currently-applied map only — anything actually
  // pending has its own record under /map/activations/, so "activation" here
  // is always the null/nothing-in-progress form.
  ss << "{\"activation\": {\"mode\": null, \"requested_time\": null, "
        "\"activation_time\": null}, \"map\": {";
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

  for (uint8_t ch = 0; ch < config_->get_alsa_channels(); ++ch) {
    if (!first_output) ss << ", ";
    first_output = false;
    ss << "\"" << is08_alsa_output_id(ch) << "\": {\"0\": ";

    std::shared_lock lock(resources_mutex_);
    auto active_it = is08_active_alsa_out_map_.find(ch);
    if (active_it != is08_active_alsa_out_map_.end()) {
      ss << "{\"input\": \"" << active_it->second.first
         << "\", \"channel_index\": " << active_it->second.second << "}";
    } else {
      ss << "{\"input\": null, \"channel_index\": null}";
    }
    ss << "}";
  }
  ss << "}}";
  return ss.str();
}

// ---------------------------------------------------------------------------
// Activation resource model (Part 2 — see plan: IS-08 activation model)
// ---------------------------------------------------------------------------

namespace {
// Resolved-and-validated form of one output-channel entry from an "action"
// object. Populated by a validation pass over the whole request before any
// mutation happens, so a single invalid entry rejects the entire activation
// instead of partially applying it.
struct Is08WorkItem {
  bool is_sender_output;
  uint8_t output_id;  // source_id (Sender) or ALSA channel number
  int output_channel;
  bool clear;  // true when "input" was null — unmap this channel
  bool input_is_sink;
  uint8_t input_id;  // sink_id or ALSA channel number
  int input_channel;
  std::string input_uuid;
};
}  // namespace

bool NmosManager::is08_apply_action_json(const std::string& action_json, std::string& err,
                                         bool dry_run, std::string* canonical_json) {
  namespace pt_ns = boost::property_tree;
  pt_ns::ptree action;
  try {
    std::istringstream ss(action_json);
    pt_ns::read_json(ss, action);
  } catch (const std::exception&) {
    err = "Could not match the request to the schema";
    return false;
  }

  std::vector<Is08WorkItem> work;
  // Hand-built per-output JSON fragments, keyed by output uuid — boost::
  // property_tree can't round-trip a plain re-serialize without quoting
  // channel_index as a string (it doesn't track JSON value types), so this
  // is built explicitly instead of via write_json.
  std::map<std::string, std::string> canonical_by_output;

  // Pass 1: resolve and validate every entry.
  for (const auto& [output_uuid, channels] : action) {
    std::ostringstream out_fragment;
    bool first_channel_fragment = true;
    uint8_t out_id = 0;
    bool is_sender_output = find_cm_output_source_id(output_uuid, out_id);
    bool is_alsa_output = !is_sender_output && find_alsa_output_channel(output_uuid, out_id);
    if (!is_sender_output && !is_alsa_output) {
      err = "Unknown output '" + output_uuid + "'";
      return false;
    }

    size_t channel_count = 1;
    if (is_sender_output) {
      StreamSource src;
      if (session_manager_->get_source(out_id, src)) {
        err = "Unknown output '" + output_uuid + "'";
        return false;
      }
      channel_count = src.map.size();
    }

    for (const auto& [channel_str, entry] : channels) {
      int output_channel;
      try {
        output_channel = std::stoi(channel_str);
      } catch (...) {
        err = "Invalid channel index '" + channel_str + "' for output '" + output_uuid + "'";
        return false;
      }
      if (output_channel < 0 || static_cast<size_t>(output_channel) >= channel_count) {
        err = "Channel index " + channel_str + " out of range for output '" + output_uuid + "'";
        return false;
      }

      // boost::property_tree's JSON parser has no null type — a JSON `null`
      // comes back as the literal string "null" (same quirk
      // patch_sender_staged/patch_receiver_staged already work around for
      // receiver_id/sender_id).
      auto input_uuid = entry.get_optional<std::string>("input");
      bool has_input = input_uuid && *input_uuid != "null" && !input_uuid->empty();

      Is08WorkItem w{};
      w.is_sender_output = is_sender_output;
      w.output_id = out_id;
      w.output_channel = output_channel;
      w.clear = !has_input;

      if (has_input) {
        int input_channel = entry.get_optional<int>("channel_index").value_or(0);
        uint8_t in_id = 0;
        bool input_is_sink = find_cm_input_sink_id(*input_uuid, in_id);
        bool input_is_alsa = !input_is_sink && find_alsa_input_channel(*input_uuid, in_id);
        if (!input_is_sink && !input_is_alsa) {
          err = "Unknown input '" + *input_uuid + "'";
          return false;
        }
        // Raw-ALSA-to-raw-ALSA isn't offered in routable_inputs (see the
        // /outputs/{id}/caps handler) — this driver has no primitive to loop
        // one physical channel to another directly, so reject it explicitly
        // rather than silently no-op'ing.
        if (!is_sender_output && input_is_alsa) {
          err = "Input '" + *input_uuid + "' is not routable to output '" + output_uuid + "'";
          return false;
        }
        if (input_is_sink) {
          StreamSink sink;
          if (session_manager_->get_sink(in_id, sink)) {
            err = "Unknown input '" + *input_uuid + "'";
            return false;
          }
          if (input_channel < 0 || static_cast<size_t>(input_channel) >= sink.map.size()) {
            err = "Channel index " + std::to_string(input_channel) +
                  " out of range for input '" + *input_uuid + "'";
            return false;
          }
        }
        w.input_is_sink = input_is_sink;
        w.input_id = in_id;
        w.input_channel = input_channel;
        w.input_uuid = *input_uuid;
      }
      work.push_back(w);

      if (!first_channel_fragment) out_fragment << ", ";
      first_channel_fragment = false;
      out_fragment << "\"" << channel_str << "\": {\"input\": "
                  << (has_input ? ("\"" + *input_uuid + "\"") : "null")
                  << ", \"channel_index\": "
                  << (has_input ? std::to_string(w.input_channel) : "null") << "}";
    }
    canonical_by_output[output_uuid] = out_fragment.str();
  }

  if (canonical_json) {
    std::ostringstream ss;
    ss << "{";
    bool first_output_fragment = true;
    for (const auto& [output_uuid, fragment] : canonical_by_output) {
      if (!first_output_fragment) ss << ", ";
      first_output_fragment = false;
      ss << "\"" << output_uuid << "\": {" << fragment << "}";
    }
    ss << "}";
    *canonical_json = ss.str();
  }

  if (dry_run) return true;

  // Pass 2: apply. Resources are re-fetched (rather than reusing pass 1's
  // copies) since a scheduled activation may fire long after validation.
  for (const auto& w : work) {
    if (w.is_sender_output) {
      StreamSource src;
      if (session_manager_->get_source(w.output_id, src)) continue;

      if (w.clear) {
        std::unique_lock lock(resources_mutex_);
        auto it = is08_active_map_.find(w.output_id);
        if (it != is08_active_map_.end()) it->second.erase(w.output_channel);
        continue;
      }

      if (w.input_is_sink) {
        StreamSink sink;
        if (session_manager_->get_sink(w.input_id, sink)) continue;
        // Repeater: a Sink's captured channel X and a Source's playback
        // channel X are the same physical ALSA channel, so activating this
        // crosspoint is just copying the ALSA channel number across — this
        // reuses the existing source-map-mutation path (the same one PUT
        // /api/source/{id} already drives).
        src.map[w.output_channel] = sink.map[w.input_channel];
      } else {
        // Direct: feed this Tx channel straight from a physical ALSA capture
        // channel, no Receiver involved.
        src.map[w.output_channel] = w.input_id;
      }
      session_manager_->add_source(src);

      std::unique_lock lock(resources_mutex_);
      is08_active_map_[w.output_id][w.output_channel] = {w.input_uuid, w.input_channel};
    } else {
      // Raw-ALSA output: it has no map of its own to mutate (only a
      // Receiver-backed Input is validated for it above). Activating this
      // crosspoint instead retargets the chosen Receiver's own Sink so that
      // its channel lands on this ALSA channel.
      if (w.clear) {
        std::unique_lock lock(resources_mutex_);
        is08_active_alsa_out_map_.erase(w.output_id);
        continue;
      }

      StreamSink sink;
      if (session_manager_->get_sink(w.input_id, sink)) continue;
      sink.map[w.input_channel] = w.output_id;
      session_manager_->add_sink(sink);

      std::unique_lock lock(resources_mutex_);
      is08_active_alsa_out_map_[w.output_id] = {w.input_uuid, w.input_channel};
    }
  }

  return true;
}

bool NmosManager::is08_output_locked(const std::string& output_id, std::string& err) const {
  std::lock_guard<std::mutex> lock(is08_activations_mutex_);
  for (const auto& [id, pa] : is08_activations_) {
    (void)id;
    if (pa.locked_outputs.count(output_id)) {
      err = "Output '" + output_id + "' is locked by another activation. No changes will be made.";
      return true;
    }
  }
  return false;
}

std::string NmosManager::is08_activation_json(const std::string& id,
                                              const Is08PendingActivation& pa) const {
  (void)id;
  std::ostringstream ss;
  ss << "{\"activation\": {\"mode\": \"" << pa.mode << "\""
     << ", \"requested_time\": "
     << (pa.requested_time.empty() ? "null" : ("\"" + pa.requested_time + "\""))
     << ", \"activation_time\": "
     << (pa.activation_time.empty() ? "null" : ("\"" + pa.activation_time + "\""))
     << "}, \"action\": " << pa.action_json << "}";
  return ss.str();
}

void NmosManager::is08_process_scheduled_activations() {
  int64_t now = is08_now_ns();

  std::vector<Is08PendingActivation> due;
  {
    std::lock_guard<std::mutex> lock(is08_activations_mutex_);
    for (auto it = is08_activations_.begin(); it != is08_activations_.end();) {
      if (it->second.deadline_ns <= now) {
        due.push_back(std::move(it->second));
        it = is08_activations_.erase(it);
      } else {
        ++it;
      }
    }
  }

  for (auto& pa : due) {
    std::string err;
    // Best-effort: re-validates against current state (a target Source/Sink
    // may have vanished since this was scheduled) — a failure here just
    // means the scheduled activation quietly doesn't happen, same leniency
    // apply()'s per-entry "continue on vanished resource" already has.
    is08_apply_action_json(pa.action_json, err);
  }
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

  // ---- Inputs (one per Sink, plus one per raw ALSA channel) ----

  nmos_get("/x-nmos/channelmapping/v1.0/inputs/", [this](const NmosReq&, NmosRes& res) {
    std::ostringstream ss;
    ss << "[";
    bool first = true;
    for (const auto& sink : session_manager_->get_sinks()) {
      if (!first) ss << ", ";
      ss << "\"" << is08_input_id(sink.id) << "/\"";
      first = false;
    }
    for (uint8_t ch = 0; ch < config_->get_alsa_channels(); ++ch) {
      if (!first) ss << ", ";
      ss << "\"" << is08_alsa_input_id(ch) << "/\"";
      first = false;
    }
    ss << "]";
    cm_ok(res, ss.str());
  });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/caps/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (!find_cm_input_sink_id(req.matches[1], id) &&
                !find_alsa_input_channel(req.matches[1], id)) { cm_not_found(res); return; }
            cm_ok(res, "{\"reordering\": false, \"block_size\": 1}");
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/parent/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (find_cm_input_sink_id(req.matches[1], id)) {
              cm_ok(res, "{\"id\": \"" + make_resource_uuid("receiver", id) +
                            "\", \"type\": \"receiver\"}");
            } else if (find_alsa_input_channel(req.matches[1], id)) {
              // No NMOS resource backs a raw ALSA channel.
              cm_ok(res, "{\"id\": null, \"type\": null}");
            } else {
              cm_not_found(res);
            }
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/channels/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (find_cm_input_sink_id(req.matches[1], id)) {
              StreamSink sink;
              if (session_manager_->get_sink(id, sink)) { cm_not_found(res); return; }
              cm_ok(res, is08_channels_json(sink.map.size()));
            } else if (find_alsa_input_channel(req.matches[1], id)) {
              cm_ok(res, is08_channels_json(1));
            } else {
              cm_not_found(res);
            }
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/inputs/([^/]+)/properties/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (find_cm_input_sink_id(req.matches[1], id)) {
              StreamSink sink;
              if (session_manager_->get_sink(id, sink)) { cm_not_found(res); return; }
              cm_ok(res, "{\"name\": \"" + sink.name + "\", \"description\": \"\"}");
            } else if (find_alsa_input_channel(req.matches[1], id)) {
              cm_ok(res, "{\"name\": \"ALSA " + std::to_string(id) + "\", \"description\": \"\"}");
            } else {
              cm_not_found(res);
            }
          });

  // ---- Outputs (one per Source, plus one per raw ALSA channel) ----

  nmos_get("/x-nmos/channelmapping/v1.0/outputs/", [this](const NmosReq&, NmosRes& res) {
    std::ostringstream ss;
    ss << "[";
    bool first = true;
    for (const auto& src : session_manager_->get_sources()) {
      if (!first) ss << ", ";
      ss << "\"" << is08_output_id(src.id) << "/\"";
      first = false;
    }
    for (uint8_t ch = 0; ch < config_->get_alsa_channels(); ++ch) {
      if (!first) ss << ", ";
      ss << "\"" << is08_alsa_output_id(ch) << "/\"";
      first = false;
    }
    ss << "]";
    cm_ok(res, ss.str());
  });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/caps/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            std::ostringstream ss;
            if (find_cm_output_source_id(req.matches[1], id)) {
              // A Sender's Tx channel can be fed by a repeated Receiver or a
              // raw ALSA capture channel.
              ss << "{\"routable_inputs\": [null";
              for (const auto& sink : session_manager_->get_sinks())
                ss << ", \"" << is08_input_id(sink.id) << "\"";
              for (uint8_t ch = 0; ch < config_->get_alsa_channels(); ++ch)
                ss << ", \"" << is08_alsa_input_id(ch) << "\"";
              ss << "]}";
            } else if (find_alsa_output_channel(req.matches[1], id)) {
              // A raw ALSA playback channel can only be fed by a Receiver —
              // ALSA-to-ALSA isn't offered: this driver has no primitive to
              // loop one physical channel to another directly (routing only
              // happens via a Source's or Sink's own map), so it can't
              // actually be realized.
              ss << "{\"routable_inputs\": [null";
              for (const auto& sink : session_manager_->get_sinks())
                ss << ", \"" << is08_input_id(sink.id) << "\"";
              ss << "]}";
            } else {
              cm_not_found(res);
              return;
            }
            cm_ok(res, ss.str());
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/sourceid/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (find_cm_output_source_id(req.matches[1], id)) {
              cm_ok(res, "\"" + make_resource_uuid("source", id) + "\"");
            } else if (find_alsa_output_channel(req.matches[1], id)) {
              cm_ok(res, "null");
            } else {
              cm_not_found(res);
            }
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/channels/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (find_cm_output_source_id(req.matches[1], id)) {
              StreamSource src;
              if (session_manager_->get_source(id, src)) { cm_not_found(res); return; }
              cm_ok(res, is08_channels_json(src.map.size()));
            } else if (find_alsa_output_channel(req.matches[1], id)) {
              cm_ok(res, is08_channels_json(1));
            } else {
              cm_not_found(res);
            }
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/outputs/([^/]+)/properties/?)",
          [this](const NmosReq& req, NmosRes& res) {
            uint8_t id;
            if (find_cm_output_source_id(req.matches[1], id)) {
              StreamSource src;
              if (session_manager_->get_source(id, src)) { cm_not_found(res); return; }
              cm_ok(res, "{\"name\": \"" + src.name + "\", \"description\": \"\"}");
            } else if (find_alsa_output_channel(req.matches[1], id)) {
              cm_ok(res, "{\"name\": \"ALSA " + std::to_string(id) + "\", \"description\": \"\"}");
            } else {
              cm_not_found(res);
            }
          });

  // ---- Map ----

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/map/active/?)", [this](const NmosReq&, NmosRes& res) {
    cm_ok(res, is08_map_active_json());
  });

  nmos_get("/x-nmos/channelmapping/v1.0/map/activations/",
          [this](const NmosReq&, NmosRes& res) {
            std::lock_guard<std::mutex> lock(is08_activations_mutex_);
            std::ostringstream ss;
            ss << "{";
            bool first = true;
            for (const auto& [id, pa] : is08_activations_) {
              if (!first) ss << ", ";
              first = false;
              ss << "\"" << id << "\": " << is08_activation_json(id, pa);
            }
            ss << "}";
            cm_ok(res, ss.str());
          });

  nmos_get(R"(/x-nmos/channelmapping/v1\.0/map/activations/([^/]+)/?)",
          [this](const NmosReq& req, NmosRes& res) {
            std::string id = req.matches[1];
            std::lock_guard<std::mutex> lock(is08_activations_mutex_);
            auto it = is08_activations_.find(id);
            if (it == is08_activations_.end()) { cm_not_found(res); return; }
            cm_ok(res, is08_activation_json(id, it->second));
          });

  nmos_delete(R"(/x-nmos/channelmapping/v1\.0/map/activations/([^/]+)/?)",
             [this](const NmosReq& req, NmosRes& res) {
               std::string id = req.matches[1];
               std::lock_guard<std::mutex> lock(is08_activations_mutex_);
               auto it = is08_activations_.find(id);
               if (it == is08_activations_.end()) { cm_not_found(res); return; }
               is08_activations_.erase(it);
               cm_no_content(res);
             });

  nmos_post(R"(/x-nmos/channelmapping/v1\.0/map/activations/?)",
           [this](const NmosReq& req, NmosRes& res) {
             namespace pt_ns = boost::property_tree;
             pt_ns::ptree pt;
             try {
               std::istringstream ss(req.body);
               pt_ns::read_json(ss, pt);
             } catch (const std::exception&) {
               cm_bad_request(res, "Could not match the request to the schema");
               return;
             }

             std::string mode = pt.get_optional<std::string>("activation.mode").value_or("");
             if (mode != "activate_immediate" && mode != "activate_scheduled_relative" &&
                 mode != "activate_scheduled_absolute") {
               cm_bad_request(res, "Could not match the request to the schema");
               return;
             }

             auto action_child = pt.get_child_optional("action");
             if (!action_child) {
               cm_bad_request(res, "Could not match the request to the schema");
               return;
             }

             // Re-serialize just the "action" subtree as parse input for
             // is08_apply_action_json below — its own canonical_json output
             // parameter is what actually gets stored/echoed (boost::
             // property_tree can't round-trip without quoting channel_index
             // as a string, since it doesn't track JSON value types).
             std::ostringstream action_ss;
             pt_ns::write_json(action_ss, *action_child, false);
             std::string raw_action_json = action_ss.str();

             // Locking: reject outright if any referenced output already has
             // a pending activation, before validating or applying anything.
             for (const auto& [output_uuid, channels] : *action_child) {
               (void)channels;
               std::string lock_err;
               if (is08_output_locked(output_uuid, lock_err)) {
                 cm_locked(res, lock_err);
                 return;
               }
             }

             std::string id = std::to_string(++is08_activation_counter_);
             std::string action_json;

             if (mode == "activate_immediate") {
               std::string err;
               if (!is08_apply_action_json(raw_action_json, err, /*dry_run=*/false, &action_json)) {
                 cm_bad_request(res, err);
                 return;
               }
               Is08PendingActivation pa;
               pa.mode = mode;
               pa.activation_time = ns_to_tai_str(is08_now_ns());
               pa.action_json = action_json;
               cm_ok(res, "{\"" + id + "\": " + is08_activation_json(id, pa) + "}");
               return;
             }

             // Scheduled: validate up front (without applying) so a bad
             // request is rejected immediately rather than silently failing
             // to no-op at fire time.
             std::string err;
             if (!is08_apply_action_json(raw_action_json, err, /*dry_run=*/true, &action_json)) {
               cm_bad_request(res, err);
               return;
             }

             std::string requested_time =
                 pt.get_optional<std::string>("activation.requested_time").value_or("");
             int64_t offset_or_absolute_ns = 0;
             if (!parse_tai_str(requested_time, offset_or_absolute_ns)) {
               cm_bad_request(res, "Invalid or missing requested_time");
               return;
             }

             int64_t deadline_ns;
             std::string activation_time;
             if (mode == "activate_scheduled_relative") {
               deadline_ns = is08_now_ns() + offset_or_absolute_ns;
               activation_time = ns_to_tai_str(deadline_ns);
             } else {
               // activate_scheduled_absolute: requested_time already is the
               // target point (system_clock-epoch basis — see the ns_to_tai_str
               // comment above; this daemon has no real leap-second TAI
               // anywhere, matching IS-05's own scheduled-absolute handling).
               deadline_ns = offset_or_absolute_ns;
               activation_time = requested_time;
             }

             Is08PendingActivation pa;
             pa.mode = mode;
             pa.requested_time = requested_time;
             pa.activation_time = activation_time;
             pa.deadline_ns = deadline_ns;
             pa.action_json = action_json;
             for (const auto& [output_uuid, channels] : *action_child) {
               (void)channels;
               pa.locked_outputs.insert(output_uuid);
             }

             {
               std::lock_guard<std::mutex> lock(is08_activations_mutex_);
               is08_activations_[id] = pa;
             }

             res.status = 202;
             cm_ok(res, "{\"" + id + "\": " + is08_activation_json(id, pa) + "}");
           });
}
