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
constexpr long kDeviceManagerOid = 2;
constexpr long kClassManagerOid = 3;
constexpr long kReceiverMonitorOidBase = 100;
constexpr long kSenderMonitorOidBase = 200;

// IS-04 interface IDs use dash-separated MAC (e.g. "aa-bb-cc-dd-ee-ff").
// PTPStatus.gmid is colon-separated; convert here. (Local copy of
// nmos_manager.cpp's file-static helper of the same name — this file follows
// the same "no cross-file helper sharing beyond NmosManager methods"
// convention already used for json_prop_descriptor/json_method_descriptor.)
std::string colon_to_dash_mac(const std::string& mac) {
  std::string out = mac;
  for (char& c : out)
    if (c == ':') c = '-';
  return out;
}

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
  bool dual_leg = !config_->get_interface_name(1).empty();

  // Both legs individually locked but to different grandmasters means
  // seamless 2022-7 switching can't be trusted even though the daemon is
  // "locked" overall - surface that as PartiallyHealthy rather than hiding
  // it behind a plain "locked".
  if (dual_leg && ptp.status == "locked" && !ptp.legs_aligned) {
    status = kHealthPartiallyHealthy;
    message = "\"Red and Blue legs are locked to different grandmasters (red: " +
              ptp.leg0_gmid + ", blue: " + ptp.leg1_gmid + ")\"";
    return;
  }

  status = ptp.status == "locked"   ? kHealthHealthy
           : ptp.status == "locking" ? kHealthPartiallyHealthy
                                     : kHealthUnhealthy;
  // The driver already fails the clock over between legs on its own
  // (Select_PTP_NIC() in manager.c) - report which one, when dual-leg is
  // actually configured, rather than just "locked".
  if (ptp.status == "locked" && dual_leg) {
    message = std::string("\"Locked via ") +
              (ptp.active_leg == 0 ? "primary (red)" : "secondary (blue)") + " leg\"";
  } else {
    message = ptp.status == "locked" ? "null" : ("\"PTP " + ptp.status + "\"");
  }
}

void NmosManager::ncp_link_status(int& status, std::string& message) const {
  bool link0_up = get_interface_link_up(config_->get_interface_name(0));
  bool has_leg2 = !config_->get_interface_name(1).empty();

  if (!has_leg2) {
    status = link0_up ? kHealthHealthy : kHealthUnhealthy;
    message = link0_up ? "null" : "\"Interface link down\"";
    return;
  }

  bool link1_up = get_interface_link_up(config_->get_interface_name(1));
  if (link0_up && link1_up) {
    status = kHealthHealthy;
    message = "null";
  } else if (link0_up || link1_up) {
    status = kHealthPartiallyHealthy;
    message = link0_up ? "\"Secondary (blue) interface link down\""
                       : "\"Primary (red) interface link down\"";
  } else {
    status = kHealthUnhealthy;
    message = "\"Both interface links down\"";
  }
}

// Updates the per-oid last-observed status + transition counters (see
// NcMonitorCounters). Safe to call from multiple contexts (Get handling and
// the once-a-second notify worker both call through the props functions
// below) since a transition is only ever counted once: the counter only
// increments when the newly observed value differs from what was already
// stored, and immediately updates that stored value.
void NmosManager::update_monitor_counters(long oid, int link, int secondary, int sync,
                                          int tertiary) const {
  std::lock_guard<std::mutex> lk(monitor_counters_mutex_);
  auto& c = monitor_counters_[oid];
  if (c.link_status != -1 && c.link_status != link) c.link_transitions++;
  if (c.secondary_status != -1 && c.secondary_status != secondary) c.secondary_transitions++;
  if (c.sync_status != -1 && c.sync_status != sync) c.sync_transitions++;
  if (c.tertiary_status != -1 && c.tertiary_status != tertiary) c.tertiary_transitions++;
  c.link_status = link;
  c.secondary_status = secondary;
  c.sync_status = sync;
  c.tertiary_status = tertiary;
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

  long oid = kReceiverMonitorOidBase + sink_id;
  props.push_back(
      {1, 7, "[{\"resourceType\": \"receiver\", \"id\": \"" + receiver_id + "\"}]"});

  int link_status = kHealthInactive, connection_status = kHealthInactive,
      sync_status = kHealthInactive, stream_status = kHealthInactive;
  std::string link_msg = "null", connection_msg = "null", sync_msg = "null", stream_msg = "null";
  std::string sync_source_id = "null";

  if (active) {
    ncp_link_status(link_status, link_msg);

    SinkStreamStatus sink_status{};
    session_manager_->get_sink_status(sink_id, sink_status);

    bool leg0_ok = sink_status.is_receiving_rtp_packet;
    bool leg0_errs = sink_status.is_rtp_seq_id_error || sink_status.is_rtp_ssrc_error ||
                     sink_status.is_rtp_payload_type_error || sink_status.is_rtp_sac_error;

    if (sink_status.leg2_present) {
      bool leg1_ok = sink_status.leg2_is_receiving_rtp_packet;
      if (leg0_ok && leg1_ok) {
        connection_status = leg0_errs ? kHealthPartiallyHealthy : kHealthHealthy;
        if (leg0_errs) connection_msg = "\"RTP stream errors detected on primary (red) leg\"";
      } else if (leg0_ok || leg1_ok) {
        connection_status = kHealthPartiallyHealthy;
        connection_msg = leg0_ok ? "\"Receiving on primary (red) leg only\""
                                 : "\"Receiving on secondary (blue) leg only\"";
      } else {
        connection_status = kHealthUnhealthy;
        connection_msg = "\"Not receiving RTP packets on either leg\"";
      }
    } else if (!leg0_ok) {
      connection_status = kHealthUnhealthy;
      connection_msg = "\"Not receiving RTP packets\"";
    } else if (leg0_errs) {
      connection_status = kHealthPartiallyHealthy;
      connection_msg = "\"RTP stream errors detected\"";
    } else {
      connection_status = kHealthHealthy;
    }

    ncp_sync_status(sync_status, sync_msg);

    PtpSyncInfo pcm = get_ptp_clock_manager_sync();
    if (pcm.available && pcm.locked) {
      sync_source_id = "\"" + pcm.gmid_dash + "\"";
    } else {
      PTPStatus ptp;
      session_manager_->get_ptp_status(ptp);
      if (ptp.status == "locked") sync_source_id = "\"" + colon_to_dash_mac(ptp.gmid) + "\"";
    }

    // No direct "is this stream still valid" query is exposed by
    // SessionManager outside its own worker loop, so streamStatus proxies
    // the sink's mute state (also carried in SinkStreamStatus) as the best
    // available signal.
    if (sink_status.is_all_muted) {
      stream_status = kHealthUnhealthy;
      stream_msg = "\"All channels muted\"";
    } else if (sink_status.is_some_muted) {
      stream_status = kHealthPartiallyHealthy;
      stream_msg = "\"Some channels muted\"";
    } else {
      stream_status = kHealthHealthy;
    }
  }

  update_monitor_counters(oid, link_status, connection_status, sync_status, stream_status);
  NcMonitorCounters counters;
  {
    std::lock_guard<std::mutex> lk(monitor_counters_mutex_);
    counters = monitor_counters_[oid];
  }

  int overall = active ? std::max({link_status, connection_status, sync_status, stream_status})
                       : kHealthInactive;

  props.push_back({3, 1, std::to_string(overall)});
  props.push_back({3, 2, "null"});
  props.push_back({4, 1, std::to_string(link_status)});
  props.push_back({4, 2, link_msg});
  props.push_back({4, 3, std::to_string(counters.link_transitions)});
  props.push_back({4, 4, std::to_string(connection_status)});
  props.push_back({4, 5, connection_msg});
  props.push_back({4, 6, std::to_string(counters.secondary_transitions)});
  props.push_back({4, 7, std::to_string(sync_status)});
  props.push_back({4, 8, sync_msg});
  props.push_back({4, 9, std::to_string(counters.sync_transitions)});
  props.push_back({4, 10, sync_source_id});
  props.push_back({4, 11, std::to_string(stream_status)});
  props.push_back({4, 12, stream_msg});
  props.push_back({4, 13, std::to_string(counters.tertiary_transitions)});
  props.push_back({4, 14, counters.auto_reset ? "true" : "false"});
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

  long oid = kSenderMonitorOidBase + source_id;
  props.push_back(
      {1, 7, "[{\"resourceType\": \"sender\", \"id\": \"" + sender_id + "\"}]"});

  int link_status = kHealthInactive, transmission_status = kHealthInactive,
      sync_status = kHealthInactive, essence_status = kHealthInactive;
  std::string link_msg = "null", transmission_msg = "null", sync_msg = "null";
  std::string sync_source_id = "null";

  if (active) {
    ncp_link_status(link_status, link_msg);

    SourceStreamStatus src_status{};
    session_manager_->get_source_status(source_id, src_status);

    bool leg0_ok = src_status.is_transmitting;
    bool leg0_underrun = src_status.is_underrun;

    if (src_status.leg2_present) {
      bool leg1_ok = src_status.leg2_is_transmitting;
      if (leg0_ok && leg1_ok) {
        transmission_status = leg0_underrun ? kHealthPartiallyHealthy : kHealthHealthy;
        if (leg0_underrun) transmission_msg = "\"Buffer underrun detected on primary (red) leg\"";
      } else if (leg0_ok || leg1_ok) {
        transmission_status = kHealthPartiallyHealthy;
        transmission_msg = leg0_ok ? "\"Transmitting on primary (red) leg only\""
                                   : "\"Transmitting on secondary (blue) leg only\"";
      } else {
        transmission_status = kHealthUnhealthy;
        transmission_msg = "\"Not transmitting RTP packets on either leg\"";
      }
    } else if (!leg0_ok) {
      transmission_status = kHealthUnhealthy;
      transmission_msg = "\"Not transmitting RTP packets\"";
    } else if (leg0_underrun) {
      transmission_status = kHealthPartiallyHealthy;
      transmission_msg = "\"Buffer underrun detected\"";
    } else {
      transmission_status = kHealthHealthy;
    }

    ncp_sync_status(sync_status, sync_msg);

    PtpSyncInfo pcm = get_ptp_clock_manager_sync();
    if (pcm.available && pcm.locked) {
      sync_source_id = "\"" + pcm.gmid_dash + "\"";
    } else {
      PTPStatus ptp;
      session_manager_->get_ptp_status(ptp);
      if (ptp.status == "locked") sync_source_id = "\"" + colon_to_dash_mac(ptp.gmid) + "\"";
    }

    // No bitstream/essence-level inspection is available in this daemon —
    // essenceStatus proxies the transmitting bit rather than any real
    // content/format validation.
    essence_status = src_status.is_transmitting ? kHealthHealthy : kHealthUnhealthy;
  }

  update_monitor_counters(oid, link_status, transmission_status, sync_status, essence_status);
  NcMonitorCounters counters;
  {
    std::lock_guard<std::mutex> lk(monitor_counters_mutex_);
    counters = monitor_counters_[oid];
  }

  int overall = active
                    ? std::max({link_status, transmission_status, sync_status, essence_status})
                    : kHealthInactive;

  props.push_back({3, 1, std::to_string(overall)});
  props.push_back({3, 2, "null"});
  props.push_back({4, 1, std::to_string(link_status)});
  props.push_back({4, 2, link_msg});
  props.push_back({4, 3, std::to_string(counters.link_transitions)});
  props.push_back({4, 4, std::to_string(transmission_status)});
  props.push_back({4, 5, transmission_msg});
  props.push_back({4, 6, std::to_string(counters.secondary_transitions)});
  props.push_back({4, 7, std::to_string(sync_status)});
  props.push_back({4, 8, sync_msg});
  props.push_back({4, 9, std::to_string(counters.sync_transitions)});
  props.push_back({4, 10, sync_source_id});
  props.push_back({4, 11, std::to_string(essence_status)});
  props.push_back({4, 12, "null"});
  props.push_back({4, 13, std::to_string(counters.tertiary_transitions)});
  props.push_back({4, 14, counters.auto_reset ? "true" : "false"});
  return props;
}

// NcDeviceManager (classId [1,3,1]) is a mandatory root singleton per
// MS-05-02, but this daemon has no real manufacturer/product/serial-number
// concept — these are static placeholder values, present so a generic
// controller's discovery walk finds a well-formed, spec-complete object
// rather than a missing/erroring one.
std::vector<NmosManager::NcPropEntry> NmosManager::ncp_device_manager_props() const {
  std::ostringstream manufacturer, product;
  manufacturer << "{\"name\": \"aes67-linux-daemon\", \"organizationId\": null, "
                  "\"website\": \"https://github.com/bondagit/aes67-linux-daemon\"}";
  product << "{\"name\": \"" << config_->get_nmos_label() << "\", \"key\": \"aes67-linux-daemon\""
          << ", \"revisionLevel\": \"1.0\", \"brandName\": \"aes67-linux-daemon\""
          << ", \"uuid\": \"" << node_id_ << "\", \"description\": \"AES67 Linux Daemon\"}";

  return {
      {3, 1, "\"v1.0.0\""},
      {3, 2, manufacturer.str()},
      {3, 3, product.str()},
      {3, 4, "\"" + node_id_ + "\""},
      {3, 5, "null"},
      {3, 6, "null"},
      {3, 7, "null"},
      {3, 8, "{\"generic\": 1, \"deviceSpecificDetails\": null}"},
      {3, 9, "0"},
      {3, 10, "null"},
  };
}

// ---------------------------------------------------------------------------
// Object model (Part 2)
// ---------------------------------------------------------------------------

std::string NmosManager::ncp_member_descriptors_json() const {
  std::ostringstream ss;
  ss << "[";
  // DeviceManager and ClassManager are mandatory members of the root block
  // per MS-05-02 — list them so a controller's GetMemberDescriptors walk
  // finds them, rather than relying on well-known oids alone.
  ss << "{\"oid\": " << kDeviceManagerOid << ", \"role\": \"DeviceManager\""
     << ", \"classId\": [1, 3, 1]}";
  ss << ", {\"oid\": " << kClassManagerOid << ", \"role\": \"ClassManager\""
     << ", \"classId\": [1, 3, 2]}";
  {
    std::shared_lock lock(resources_mutex_);
    for (const auto& [id, rr] : receivers_) {
      (void)rr;
      ss << ", {\"oid\": " << (kReceiverMonitorOidBase + id) << ", \"role\": \"ReceiverMonitor"
         << +id << "\", \"classId\": [1, 2, 2, 1]}";
    }
    for (const auto& [id, sr] : senders_) {
      (void)sr;
      ss << ", {\"oid\": " << (kSenderMonitorOidBase + id) << ", \"role\": \"SenderMonitor" << +id
         << "\", \"classId\": [1, 2, 2, 2]}";
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
  } else if (class_id == std::vector<int>{1, 3, 1}) {
    name = "NcDeviceManager";
    properties.push_back(json_prop_descriptor(3, 1, "ncVersion", "NcString", true, false, false));
    properties.push_back(json_prop_descriptor(3, 2, "manufacturer", "NcManufacturer", true, false, false));
    properties.push_back(json_prop_descriptor(3, 3, "product", "NcProduct", true, false, false));
    properties.push_back(json_prop_descriptor(3, 4, "serialNumber", "NcString", true, false, false));
    properties.push_back(json_prop_descriptor(3, 5, "userInventoryCode", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(3, 6, "deviceName", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(3, 7, "deviceRole", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(3, 8, "operationalState", "NcDeviceOperationalState", true, false, false));
    properties.push_back(json_prop_descriptor(3, 9, "resetCause", "NcResetCause", true, false, false));
    properties.push_back(json_prop_descriptor(3, 10, "message", "NcString", true, true, false));
  } else if (class_id == std::vector<int>{1, 2, 2, 1}) {
    // Property numbering matches BCP-008-01's registered NcReceiverMonitor
    // layout exactly (verified against AMWA's own nmos-device-control-mock
    // reference implementation) so a controller that hardcodes the standard
    // ids for this well-known classId reads the right fields.
    name = "NcReceiverMonitor";
    properties.push_back(json_prop_descriptor(1, 7, "touchpoints", "NcTouchpoint", true, true, true));
    properties.push_back(json_prop_descriptor(3, 1, "overallStatus", "NcOverallStatus", true, false, false));
    properties.push_back(json_prop_descriptor(3, 2, "overallStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 1, "linkStatus", "NcLinkStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 2, "linkStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 3, "linkStatusTransitionCounter", "NcUint32", true, false, false));
    properties.push_back(json_prop_descriptor(4, 4, "connectionStatus", "NcConnectionStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 5, "connectionStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 6, "connectionStatusTransitionCounter", "NcUint32", true, false, false));
    properties.push_back(json_prop_descriptor(4, 7, "externalSynchronizationStatus", "NcSynchronizationStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 8, "externalSynchronizationStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 9, "externalSynchronizationStatusTransitionCounter", "NcUint32", true, false, false));
    properties.push_back(json_prop_descriptor(4, 10, "synchronizationSourceId", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 11, "streamStatus", "NcStreamStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 12, "streamStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 13, "streamStatusTransitionCounter", "NcUint32", true, false, false));
    properties.push_back(json_prop_descriptor(4, 14, "autoResetCountersAndMessages", "NcBoolean", false, false, false));
  } else if (class_id == std::vector<int>{1, 2, 2, 2}) {
    name = "NcSenderMonitor";
    properties.push_back(json_prop_descriptor(1, 7, "touchpoints", "NcTouchpoint", true, true, true));
    properties.push_back(json_prop_descriptor(3, 1, "overallStatus", "NcOverallStatus", true, false, false));
    properties.push_back(json_prop_descriptor(3, 2, "overallStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 1, "linkStatus", "NcLinkStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 2, "linkStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 3, "linkStatusTransitionCounter", "NcUint32", true, false, false));
    properties.push_back(json_prop_descriptor(4, 4, "transmissionStatus", "NcTransmissionStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 5, "transmissionStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 6, "transmissionStatusTransitionCounter", "NcUint32", true, false, false));
    properties.push_back(json_prop_descriptor(4, 7, "externalSynchronizationStatus", "NcSynchronizationStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 8, "externalSynchronizationStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 9, "externalSynchronizationStatusTransitionCounter", "NcUint32", true, false, false));
    properties.push_back(json_prop_descriptor(4, 10, "synchronizationSourceId", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 11, "essenceStatus", "NcEssenceStatus", true, false, false));
    properties.push_back(json_prop_descriptor(4, 12, "essenceStatusMessage", "NcString", true, true, false));
    properties.push_back(json_prop_descriptor(4, 13, "essenceStatusTransitionCounter", "NcUint32", true, false, false));
    properties.push_back(json_prop_descriptor(4, 14, "autoResetCountersAndMessages", "NcBoolean", false, false, false));
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
        if (oid == kDeviceManagerOid) {
          props = ncp_device_manager_props();
        } else if (oid >= kReceiverMonitorOidBase && oid < kReceiverMonitorOidBase + 64) {
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
        // NcObject.Set — every property this daemon exposes is read-only
        // except autoResetCountersAndMessages (4p14) on a monitor oid.
        int plevel = args ? args->get<int>("id.level", 0) : 0;
        int pindex = args ? args->get<int>("id.index", 0) : 0;
        bool is_monitor_oid = (oid >= kReceiverMonitorOidBase && oid < kReceiverMonitorOidBase + 64) ||
                              (oid >= kSenderMonitorOidBase && oid < kSenderMonitorOidBase + 64);

        if (is_monitor_oid && plevel == 4 && pindex == 14) {
          bool value = args ? args->get<bool>("value", true) : true;
          {
            std::lock_guard<std::mutex> lk(monitor_counters_mutex_);
            monitor_counters_[oid].auto_reset = value;
          }
          status = 200;
          error_message.clear();
        } else {
          status = 405;
          error_message = "PropertyReadOnly";
        }
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
