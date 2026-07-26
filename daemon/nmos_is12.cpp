//
//  nmos_is12.cpp
//
//  IS-12 (NMOS Control Protocol) WebSocket server, implementing just enough
//  of the MS-05-02 device model for BCP-008-01/02 (Receiver/Sender Status
//  Monitoring): a root NcBlock (oid 1), an NcClassManager (oid 3) answering
//  GetControlClass, and one NcReceiverMonitor per Sink / NcSenderMonitor per
//  Source. Health values are computed on demand (Get) and diffed once a
//  second to drive Notifications to subscribed clients.
//
//  Methods defined here are all members of NmosManager (declared in
//  nmos_manager.hpp) — split into this file purely to keep nmos_manager.cpp
//  from growing further, matching this codebase's "one class, per-spec
//  implementation file" convention used for nmos_manager.cpp itself.
//

#include <algorithm>
#include <chrono>
#include <sstream>
#include <thread>

#include <boost/asio/buffer.hpp>
#include <boost/beast/core/buffers_to_string.hpp>
#include <boost/property_tree/json_parser.hpp>
#include <boost/property_tree/ptree.hpp>

#include "interface.hpp"
#include "log.hpp"
#include "nmos_manager.hpp"

namespace {
// NcOverallStatus/NcLinkStatus/NcConnectionStatus/NcSynchronizationStatus/
// NcStreamStatus/NcTransmissionStatus/NcEssenceStatus all share this ordering
// per BCP-008: since the daemon controls both ends of the wire protocol here
// (unlike the nmosrouter client, which could only guess), these values are
// authoritative rather than a guess.
constexpr int kHealthInactive = 0;
constexpr int kHealthHealthy = 1;
constexpr int kHealthPartiallyHealthy = 2;
constexpr int kHealthUnhealthy = 3;

constexpr long kRootBlockOid = 1;
constexpr long kClassManagerOid = 3;
constexpr long kReceiverMonitorOidBase = 100;
constexpr long kSenderMonitorOidBase = 200;

std::string json_prop_descriptor(int level, int index, const std::string& name,
                                 const std::string& type_name, bool is_read_only,
                                 bool is_nullable, bool is_sequence) {
  std::ostringstream ss;
  ss << "{\"id\": {\"level\": " << level << ", \"index\": " << index << "}"
     << ", \"name\": \"" << name << "\""
     << ", \"typeName\": \"" << type_name << "\""
     << ", \"isReadOnly\": " << std::boolalpha << is_read_only
     << ", \"isNullable\": " << std::boolalpha << is_nullable
     << ", \"isSequence\": " << std::boolalpha << is_sequence << "}";
  return ss.str();
}

std::string json_method_descriptor(int level, int index, const std::string& name) {
  std::ostringstream ss;
  ss << "{\"id\": {\"level\": " << level << ", \"index\": " << index << "}"
     << ", \"name\": \"" << name << "\"}";
  return ss.str();
}
}  // namespace

// ---------------------------------------------------------------------------
// BCP-008 status computation (Part 3)
// ---------------------------------------------------------------------------

// externalSynchronizationStatus for both receiver and sender monitors:
// prefers ptp-clock-manager's discipline state (locked/locking against the
// grandmaster, with measured offset) over the driver's raw PTP message
// reception when ptp-clock-manager is running — see
// NmosManager::get_ptp_clock_manager_sync.
void NmosManager::ncp_sync_status(int& status, std::string& message) const {
  PtpSyncInfo pcm = get_ptp_clock_manager_sync();
  if (pcm.available) {
    status = pcm.locked            ? kHealthHealthy
             : pcm.locking         ? kHealthPartiallyHealthy
                                    : kHealthUnhealthy;
    if (pcm.locked) {
      message = "null";
    } else {
      std::ostringstream m;
      m << "\"PTP " << (pcm.locking ? "locking" : "unlocked") << ", offset "
        << pcm.offset_ns << "ns\"";
      message = m.str();
    }
    return;
  }

  PTPStatus ptp;
  session_manager_->get_ptp_status(ptp);
  status = ptp.status == "locked"   ? kHealthHealthy
           : ptp.status == "locking" ? kHealthPartiallyHealthy
                                     : kHealthUnhealthy;
  message = ptp.status == "locked" ? "null" : ("\"PTP " + ptp.status + "\"");
}

std::vector<NmosManager::NcPropEntry> NmosManager::ncp_receiver_monitor_props(
    uint8_t sink_id) const {
  std::vector<NcPropEntry> props;

  bool active = false;
  std::string receiver_id;
  {
    std::shared_lock lock(resources_mutex_);
    auto it = receivers_.find(sink_id);
    if (it == receivers_.end()) return props;
    active = it->second.active_master_enable;
    receiver_id = it->second.receiver_id;
  }

  props.push_back(
      {1, 7, "[{\"resourceType\": \"receiver\", \"id\": \"" + receiver_id + "\"}]"});

  if (!active) {
    props.push_back({3, 1, std::to_string(kHealthInactive)});
    props.push_back({3, 2, "null"});
    props.push_back({4, 1, std::to_string(kHealthInactive)});
    props.push_back({4, 2, "null"});
    props.push_back({4, 3, std::to_string(kHealthInactive)});
    props.push_back({4, 4, "null"});
    props.push_back({4, 5, std::to_string(kHealthInactive)});
    props.push_back({4, 6, "null"});
    props.push_back({4, 7, std::to_string(kHealthInactive)});
    props.push_back({4, 8, "null"});
    return props;
  }

  bool link_up = get_interface_link_up(config_->get_interface_name());
  int link_status = link_up ? kHealthHealthy : kHealthUnhealthy;

  SinkStreamStatus sink_status{};
  session_manager_->get_sink_status(sink_id, sink_status);

  int connection_status;
  std::string connection_msg = "null";
  if (!sink_status.is_receiving_rtp_packet) {
    connection_status = kHealthUnhealthy;
    connection_msg = "\"Not receiving RTP packets\"";
  } else if (sink_status.is_rtp_seq_id_error || sink_status.is_rtp_ssrc_error ||
             sink_status.is_rtp_payload_type_error || sink_status.is_rtp_sac_error) {
    connection_status = kHealthPartiallyHealthy;
    connection_msg = "\"RTP stream errors detected\"";
  } else {
    connection_status = kHealthHealthy;
  }

  int sync_status;
  std::string sync_msg;
  ncp_sync_status(sync_status, sync_msg);

  // No direct "is this stream still valid" query is exposed by SessionManager
  // outside its own worker loop, so streamStatus proxies the sink's mute
  // state (also carried in SinkStreamStatus) as the best available signal.
  int stream_status;
  std::string stream_msg = "null";
  if (sink_status.is_all_muted) {
    stream_status = kHealthUnhealthy;
    stream_msg = "\"All channels muted\"";
  } else if (sink_status.is_some_muted) {
    stream_status = kHealthPartiallyHealthy;
    stream_msg = "\"Some channels muted\"";
  } else {
    stream_status = kHealthHealthy;
  }

  int overall = std::max({link_status, connection_status, sync_status, stream_status});

  props.push_back({3, 1, std::to_string(overall)});
  props.push_back({3, 2, "null"});
  props.push_back({4, 1, std::to_string(link_status)});
  props.push_back({4, 2, link_up ? "null" : "\"Interface link down\""});
  props.push_back({4, 3, std::to_string(connection_status)});
  props.push_back({4, 4, connection_msg});
  props.push_back({4, 5, std::to_string(sync_status)});
  props.push_back({4, 6, sync_msg});
  props.push_back({4, 7, std::to_string(stream_status)});
  props.push_back({4, 8, stream_msg});
  return props;
}

std::vector<NmosManager::NcPropEntry> NmosManager::ncp_sender_monitor_props(
    uint8_t source_id) const {
  std::vector<NcPropEntry> props;

  bool active = false;
  std::string sender_id;
  {
    std::shared_lock lock(resources_mutex_);
    auto it = senders_.find(source_id);
    if (it == senders_.end()) return props;
    active = it->second.active_master_enable;
    sender_id = it->second.sender_id;
  }

  props.push_back(
      {1, 7, "[{\"resourceType\": \"sender\", \"id\": \"" + sender_id + "\"}]"});

  if (!active) {
    props.push_back({3, 1, std::to_string(kHealthInactive)});
    props.push_back({3, 2, "null"});
    props.push_back({4, 1, std::to_string(kHealthInactive)});
    props.push_back({4, 2, "null"});
    props.push_back({4, 3, std::to_string(kHealthInactive)});
    props.push_back({4, 4, "null"});
    props.push_back({4, 5, std::to_string(kHealthInactive)});
    props.push_back({4, 6, "null"});
    props.push_back({4, 7, std::to_string(kHealthInactive)});
    props.push_back({4, 8, "null"});
    return props;
  }

  bool link_up = get_interface_link_up(config_->get_interface_name());
  int link_status = link_up ? kHealthHealthy : kHealthUnhealthy;

  SourceStreamStatus src_status{};
  session_manager_->get_source_status(source_id, src_status);

  int transmission_status;
  std::string transmission_msg = "null";
  if (!src_status.is_transmitting) {
    transmission_status = kHealthUnhealthy;
    transmission_msg = "\"Not transmitting RTP packets\"";
  } else if (src_status.is_underrun) {
    transmission_status = kHealthPartiallyHealthy;
    transmission_msg = "\"Buffer underrun detected\"";
  } else {
    transmission_status = kHealthHealthy;
  }

  int sync_status;
  std::string sync_msg;
  ncp_sync_status(sync_status, sync_msg);

  // No bitstream/essence-level inspection is available in this daemon —
  // essenceStatus proxies the transmitting bit rather than any real
  // content/format validation.
  int essence_status = src_status.is_transmitting ? kHealthHealthy : kHealthUnhealthy;

  int overall = std::max({link_status, transmission_status, sync_status, essence_status});

  props.push_back({3, 1, std::to_string(overall)});
  props.push_back({3, 2, "null"});
  props.push_back({4, 1, std::to_string(link_status)});
  props.push_back({4, 2, link_up ? "null" : "\"Interface link down\""});
  props.push_back({4, 3, std::to_string(transmission_status)});
  props.push_back({4, 4, transmission_msg});
  props.push_back({4, 5, std::to_string(sync_status)});
  props.push_back({4, 6, sync_msg});
  props.push_back({4, 7, std::to_string(essence_status)});
  props.push_back({4, 8, "null"});
  return props;
}

// ---------------------------------------------------------------------------
// Object model (Part 2)
// ---------------------------------------------------------------------------

std::string NmosManager::ncp_member_descriptors_json() const {
  std::ostringstream ss;
  ss << "[";
  bool first = true;
  {
    std::shared_lock lock(resources_mutex_);
    for (const auto& [id, rr] : receivers_) {
      if (!first) ss << ", ";
      ss << "{\"oid\": " << (kReceiverMonitorOidBase + id) << ", \"role\": \"ReceiverMonitor"
         << +id << "\", \"classId\": [1, 2, 2, 1]}";
      first = false;
    }
    for (const auto& [id, sr] : senders_) {
      if (!first) ss << ", ";
      ss << "{\"oid\": " << (kSenderMonitorOidBase + id) << ", \"role\": \"SenderMonitor" << +id
         << "\", \"classId\": [1, 2, 2, 2]}";
      first = false;
    }
  }
  ss << "]";
  return ss.str();
}

bool NmosManager::ncp_class_descriptor_json(const std::vector<int>& class_id,
                                            std::string& out) const {
  std::string name;
  std::vector<std::string> properties;
  std::vector<std::string> methods;

  if (class_id == std::vector<int>{1, 1}) {
    name = "NcBlock";
    methods.push_back(json_method_descriptor(2, 1, "GetMemberDescriptors"));
  } else if (class_id == std::vector<int>{1, 3, 2}) {
    name = "NcClassManager";
    methods.push_back(json_method_descriptor(3, 1, "GetControlClass"));
  } else if (class_id == std::vector<int>{1, 2, 2}) {
    name = "NcStatusMonitor";
    properties.push_back(json_prop_descriptor(3, 1, "overallStatus", "NcOverallStatus", true, false, false));
    properties.push_back(json_prop_descriptor(3, 2, "overallStatusMessage", "NcString", true, true, false));
  } else if (class_id == std::vector<int>{1, 2, 2, 1}) {
    name = "NcReceiverMonitor";
    properties.push_back(json_prop_descriptor(1, 7, "touchpoints", "NcTouchpoint", true, true, true));
    properties.push_back(json_prop_descriptor(3, 1, "overallStatus", "NcOverallStatus", true, false, false));
    properties.push_back(json_prop_descriptor(3, 2, "overallStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 1, "linkStatus", "NcLinkStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 2, "linkStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 3, "connectionStatus", "NcConnectionStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 4, "connectionStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 5, "externalSynchronizationStatus", "NcSynchronizationStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 6, "externalSynchronizationStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 7, "streamStatus", "NcStreamStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 8, "streamStatusMessage", "NcString", true, true, false));
  } else if (class_id == std::vector<int>{1, 2, 2, 2}) {
    name = "NcSenderMonitor";
    properties.push_back(json_prop_descriptor(1, 7, "touchpoints", "NcTouchpoint", true, true, true));
    properties.push_back(json_prop_descriptor(3, 1, "overallStatus", "NcOverallStatus", true, false, false));
    properties.push_back(json_prop_descriptor(3, 2, "overallStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 1, "linkStatus", "NcLinkStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 2, "linkStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 3, "transmissionStatus", "NcTransmissionStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 4, "transmissionStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 5, "externalSynchronizationStatus", "NcSynchronizationStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 6, "externalSynchronizationStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 7, "essenceStatus", "NcEssenceStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 8, "essenceStatusMessage", "NcString", true, true, false));
  } else {
    return false;
  }

  std::ostringstream ss;
  ss << "{\"classId\": [";
  for (size_t i = 0; i < class_id.size(); ++i) {
    if (i) ss << ", ";
    ss << class_id[i];
  }
  ss << "], \"name\": \"" << name << "\", \"properties\": [";
  for (size_t i = 0; i < properties.size(); ++i) {
    if (i) ss << ", ";
    ss << properties[i];
  }
  ss << "], \"methods\": [";
  for (size_t i = 0; i < methods.size(); ++i) {
    if (i) ss << ", ";
    ss << methods[i];
  }
  ss << "]}";
  out = ss.str();
  return true;
}

// ---------------------------------------------------------------------------
// Wire protocol dispatch
// ---------------------------------------------------------------------------

void NmosManager::handle_is12_message(const std::string& msg,
                                      const std::shared_ptr<Is12Session>& session) {
  namespace pt_ns = boost::property_tree;
  pt_ns::ptree pt;
  try {
    std::istringstream ss(msg);
    pt_ns::read_json(ss, pt);
  } catch (const std::exception& e) {
    BOOST_LOG_TRIVIAL(debug) << "NmosManager:: IS-12 malformed message: " << e.what();
    return;
  }

  int message_type = pt.get<int>("messageType", -1);

  if (message_type == 0) {  // Command
    auto commands = pt.get_child_optional("commands");
    if (!commands) return;

    std::ostringstream resp;
    resp << "{\"messageType\": 1, \"responses\": [";
    bool first = true;
    for (const auto& [key, cmd] : *commands) {
      (void)key;
      int handle = cmd.get<int>("handle", 0);
      long oid = cmd.get<long>("oid", 0);
      int mlevel = cmd.get<int>("methodId.level", 0);
      int mindex = cmd.get<int>("methodId.index", 0);
      auto args = cmd.get_child_optional("arguments");

      int status = 501;
      std::string value_json = "null";
      std::string error_message = "MethodNotImplemented";

      if (oid == kRootBlockOid && mlevel == 2 && mindex == 1) {
        // NcBlock.GetMemberDescriptors — this daemon has no nested blocks,
        // so "recurse" makes no difference; always return the flat list.
        status = 200;
        error_message.clear();
        value_json = ncp_member_descriptors_json();
      } else if (oid == kClassManagerOid && mlevel == 3 && mindex == 1) {
        // NcClassManager.GetControlClass
        std::vector<int> class_id;
        if (args) {
          auto cid = args->get_child_optional("classId");
          if (cid)
            for (const auto& [k2, v2] : *cid) {
              (void)k2;
              class_id.push_back(v2.get_value<int>());
            }
        }
        std::string descriptor_json;
        if (!class_id.empty() && ncp_class_descriptor_json(class_id, descriptor_json)) {
          status = 200;
          error_message.clear();
          value_json = descriptor_json;
        } else {
          status = 404;
          error_message = "Unknown classId";
        }
      } else if (mlevel == 1 && mindex == 1) {
        // NcObject.Get
        int plevel = args ? args->get<int>("id.level", 0) : 0;
        int pindex = args ? args->get<int>("id.index", 0) : 0;

        std::vector<NcPropEntry> props;
        if (oid >= kReceiverMonitorOidBase && oid < kReceiverMonitorOidBase + 64) {
          props = ncp_receiver_monitor_props(static_cast<uint8_t>(oid - kReceiverMonitorOidBase));
        } else if (oid >= kSenderMonitorOidBase && oid < kSenderMonitorOidBase + 64) {
          props = ncp_sender_monitor_props(static_cast<uint8_t>(oid - kSenderMonitorOidBase));
        }

        auto it = std::find_if(props.begin(), props.end(), [&](const NcPropEntry& p) {
          return p.level == plevel && p.index == pindex;
        });
        if (it != props.end()) {
          status = 200;
          error_message.clear();
          value_json = it->json_value;
        } else {
          status = props.empty() ? 404 : 501;
          error_message = props.empty() ? "Unknown oid" : "PropertyNotImplemented";
        }
      } else if (mlevel == 1 && mindex == 2) {
        // NcObject.Set — every property this daemon exposes is read-only.
        status = 405;
        error_message = "PropertyReadOnly";
      }

      if (!first) resp << ", ";
      resp << "{\"handle\": " << handle << ", \"result\": {\"status\": " << status
           << ", \"value\": " << value_json << ", \"errorMessage\": "
           << (error_message.empty() ? "null" : ("\"" + error_message + "\"")) << "}}";
      first = false;
    }
    resp << "]}";

    std::lock_guard<std::mutex> lk(session->mtx);
    session->outbox.push_back(resp.str());
    session->cv.notify_all();
  } else if (message_type == 3) {  // Subscription
    auto subs = pt.get_child_optional("subscriptions");
    std::set<long> new_subs;
    if (subs)
      for (const auto& [k, v] : *subs) {
        (void)k;
        new_subs.insert(v.get_value<long>());
      }

    std::ostringstream resp;
    resp << "{\"messageType\": 4, \"subscriptions\": [";
    bool first = true;
    for (long o : new_subs) {
      if (!first) resp << ", ";
      resp << o;
      first = false;
    }
    resp << "]}";

    std::lock_guard<std::mutex> lk(session->mtx);
    session->subscribed = std::move(new_subs);
    session->outbox.push_back(resp.str());
    session->cv.notify_all();
  }
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

void NmosManager::serve_is12_connection(
    boost::beast::websocket::stream<boost::beast::tcp_stream>& ws) {
  namespace net = boost::asio;
  namespace beast = boost::beast;
  namespace websocket = beast::websocket;

  auto session = std::make_shared<Is12Session>();
  {
    std::lock_guard<std::mutex> lk(is12_sessions_mutex_);
    is12_sessions_.push_back(session);
  }

  // Companion writer thread: the only thread allowed to call ws.write() for
  // this connection, draining both Command responses and async Notifications
  // from one outbox so the two never race to write concurrently. The read
  // loop below runs concurrently on the calling thread — Boost.Beast permits
  // exactly one outstanding read and one outstanding write on the same
  // stream at the same time.
  std::thread writer([&ws, session]() {
    namespace net_w = boost::asio;
    while (true) {
      std::string msg;
      {
        std::unique_lock<std::mutex> lk(session->mtx);
        session->cv.wait(lk, [&] { return session->closing || !session->outbox.empty(); });
        if (session->outbox.empty()) {
          if (session->closing) break;
          continue;
        }
        msg = std::move(session->outbox.front());
        session->outbox.pop_front();
      }
      try {
        ws.text(true);
        boost::system::error_code ec;
        ws.write(net_w::buffer(msg), ec);
        if (ec) break;
      } catch (...) {
        break;
      }
    }
  });

  BOOST_LOG_TRIVIAL(debug) << "NmosManager:: IS-12 client connected";
  while (running_) {
    beast::flat_buffer rbuf;
    boost::system::error_code ec;
    ws.read(rbuf, ec);
    if (ec == websocket::error::closed || ec) break;
    handle_is12_message(beast::buffers_to_string(rbuf.data()), session);
  }
  BOOST_LOG_TRIVIAL(debug) << "NmosManager:: IS-12 client disconnected";

  {
    std::lock_guard<std::mutex> lk(session->mtx);
    session->closing = true;
  }
  session->cv.notify_all();
  writer.join();

  {
    std::lock_guard<std::mutex> lk(is12_sessions_mutex_);
    is12_sessions_.erase(std::remove(is12_sessions_.begin(), is12_sessions_.end(), session),
                         is12_sessions_.end());
  }
}

// ---------------------------------------------------------------------------
// Notification worker: diffs computed monitor properties once a second and
// pushes Notifications to any session subscribed to a changed oid.
// ---------------------------------------------------------------------------

bool NmosManager::is12_notify_worker() {
  std::map<long, std::vector<NcPropEntry>> last_known;

  while (running_) {
    std::this_thread::sleep_for(std::chrono::seconds(1));
    if (!running_) break;

    std::map<uint8_t, ReceiverResources> receivers_copy;
    std::map<uint8_t, SenderResources> senders_copy;
    {
      std::shared_lock lock(resources_mutex_);
      receivers_copy = receivers_;
      senders_copy = senders_;
    }

    std::vector<std::pair<long, std::vector<NcPropEntry>>> current;
    for (const auto& [id, rr] : receivers_copy) {
      (void)rr;
      current.emplace_back(kReceiverMonitorOidBase + id, ncp_receiver_monitor_props(id));
    }
    for (const auto& [id, sr] : senders_copy) {
      (void)sr;
      current.emplace_back(kSenderMonitorOidBase + id, ncp_sender_monitor_props(id));
    }

    std::vector<std::shared_ptr<Is12Session>> sessions_copy;
    {
      std::lock_guard<std::mutex> lk(is12_sessions_mutex_);
      sessions_copy = is12_sessions_;
    }

    for (const auto& [oid, props] : current) {
      auto prev_it = last_known.find(oid);
      bool have_prev = prev_it != last_known.end();

      for (const auto& p : props) {
        bool changed = true;
        if (have_prev) {
          auto match = std::find_if(
              prev_it->second.begin(), prev_it->second.end(),
              [&](const NcPropEntry& pp) { return pp.level == p.level && pp.index == p.index; });
          changed = (match == prev_it->second.end()) || (match->json_value != p.json_value);
        }
        // Only notify once a prior baseline exists — otherwise every monitor
        // would fire a Notification storm the instant the worker starts.
        if (changed && have_prev && !sessions_copy.empty()) {
          std::ostringstream notif;
          notif << "{\"messageType\": 2, \"notifications\": [{\"oid\": " << oid
                << ", \"eventId\": {\"level\": 1, \"index\": 1}, \"eventData\": "
                << "{\"propertyId\": {\"level\": " << p.level << ", \"index\": " << p.index
                << "}, \"changeType\": 0, \"value\": " << p.json_value
                << ", \"sequenceItemIndex\": null}}]}";
          std::string notif_str = notif.str();
          for (auto& sess : sessions_copy) {
            std::lock_guard<std::mutex> lk(sess->mtx);
            if (sess->subscribed.count(oid)) {
              sess->outbox.push_back(notif_str);
              sess->cv.notify_all();
            }
          }
        }
      }
      last_known[oid] = props;
    }

    // Drop entries for sinks/sources that no longer exist so this map
    // doesn't grow across add/remove churn.
    for (auto it = last_known.begin(); it != last_known.end();) {
      bool still_exists = std::any_of(current.begin(), current.end(),
                                      [&](const auto& c) { return c.first == it->first; });
      if (!still_exists)
        it = last_known.erase(it);
      else
        ++it;
    }
  }
  return true;
}
