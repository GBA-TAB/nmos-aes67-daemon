//
//  nmos_manager.cpp
//
//  IS-04 Node API + Registration client implementation.
//  One Node, one Device, one Sender+Source+Flow per StreamSource,
//  one Receiver per StreamSink.
//

#include <arpa/inet.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include <chrono>
#include <sstream>
#include <string>
#include <thread>

#include <boost/asio/ip/tcp.hpp>
#include <boost/beast/core.hpp>
#include <boost/beast/http.hpp>
#include <boost/beast/websocket.hpp>

#include <boost/property_tree/json_parser.hpp>
#include <boost/property_tree/ptree.hpp>
#include <boost/version.hpp>
#if BOOST_VERSION >= 106700
#  include <boost/uuid/name_generator_sha1.hpp>
#else
#  include <boost/uuid/name_generator.hpp>
#endif
#include <boost/uuid/string_generator.hpp>
#include <boost/uuid/uuid.hpp>
#include <boost/uuid/uuid_io.hpp>

#include <httplib.h>

#include "interface.hpp"
#include "log.hpp"
#include "nmos_manager.hpp"

#ifdef _USE_AVAHI_
#include <avahi-common/address.h>
#endif

// ---------------------------------------------------------------------------
// Static helpers
// ---------------------------------------------------------------------------

// IS-04 interface IDs use dash-separated MAC (e.g. "aa-bb-cc-dd-ee-ff").
// Config returns colon-separated; convert here.
static std::string colon_to_dash_mac(const std::string& mac) {
  std::string out = mac;
  for (char& c : out)
    if (c == ':') c = '-';
  return out;
}

// Formats an 8-byte EUI-64 (e.g. PtpClockShm::gmid) as IS-04's dash-separated
// hex form, "xx-xx-xx-xx-xx-xx-xx-xx".
static std::string eui64_to_dash_hex(const uint8_t bytes[8]) {
  static const char* hex = "0123456789abcdef";
  std::string out;
  out.reserve(23);
  for (int i = 0; i < 8; ++i) {
    if (i) out += '-';
    out += hex[(bytes[i] >> 4) & 0xf];
    out += hex[bytes[i] & 0xf];
  }
  return out;
}

static std::string get_system_hostname() {
  char buf[256];
  if (gethostname(buf, sizeof(buf)) == 0) {
    buf[sizeof(buf) - 1] = '\0';
    return buf;
  }
  return "localhost";
}

// Build NMOS label: "<hostname> ALSA <first>-<last>" using 1-indexed channel numbers.
static std::string make_nmos_label(const std::vector<uint8_t>& map) {
  std::string h = get_system_hostname();
  if (map.empty()) return h + " ALSA";
  return h + " ALSA " + std::to_string(map.front() + 1)
         + "-" + std::to_string(map.back() + 1);
}

static std::string make_uuid5(const std::string& ns_str, const std::string& name) {
  boost::uuids::string_generator sgen;
  boost::uuids::uuid ns = sgen(ns_str);
#if BOOST_VERSION >= 106700
  boost::uuids::name_generator_sha1 ngen(ns);
#else
  boost::uuids::name_generator ngen(ns);
#endif
  return boost::uuids::to_string(ngen(name));
}

static std::string make_version() {
  auto now = std::chrono::system_clock::now().time_since_epoch();
  auto secs = std::chrono::duration_cast<std::chrono::seconds>(now).count();
  auto nanos = std::chrono::duration_cast<std::chrono::nanoseconds>(now).count() %
               1'000'000'000LL;
  return std::to_string(secs) + ":" + std::to_string(nanos);
}

// IS-04 grain timestamps use TAI (UTC + 37s since Jan 2017)
static std::string make_tai_timestamp() {
  struct timespec ts;
  clock_gettime(CLOCK_REALTIME, &ts);
  ts.tv_sec += 37;
  return std::to_string(ts.tv_sec) + ":" + std::to_string(ts.tv_nsec);
}

static bool is_multicast(const std::string& addr) {
  struct in_addr a {};
  if (inet_pton(AF_INET, addr.c_str(), &a) == 1) {
    uint32_t ip = ntohl(a.s_addr);
    return ip >= 0xE0000000u && ip <= 0xEFFFFFFFu;
  }
  return false;
}

static void codec_to_nmos(const std::string& codec,
                           std::string& media_type,
                           int& bit_depth) {
  if (codec == "L16") {
    media_type = "audio/L16";
    bit_depth = 16;
  } else if (codec == "AM824") {
    media_type = "audio/AM824";
    bit_depth = 32;
  } else {
    media_type = "audio/L24";
    bit_depth = 24;
  }
}

static std::string make_channels_json(const std::vector<uint8_t>& map) {
  std::ostringstream ss;
  ss << "[";
  size_t n = map.size();
  for (size_t i = 0; i < n; ++i) {
    if (i > 0) ss << ", ";
    ss << "{\"label\": \"";
    if (n == 2) {
      ss << (i == 0 ? "Left" : "Right");
    } else {
      ss << "Ch" << (i + 1);
    }
    ss << "\"}";
  }
  ss << "]";
  return ss.str();
}

using NmosReq = NmosManager::NmosReq;
using NmosRes = NmosManager::NmosRes;

static void set_nmos_headers(NmosRes& res) {
  res.set_header("Access-Control-Allow-Origin", "*");
  res.set_header("Access-Control-Allow-Methods", "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS");
  res.set_header("Access-Control-Allow-Headers", "Content-Type, Accept");
  res.set_header("Cache-Control", "no-cache, no-store");
}

static void nmos_ok(NmosRes& res, const std::string& body) {
  set_nmos_headers(res);
  res.set_content(body, "application/json");
}

static void nmos_not_found(NmosRes& res) {
  set_nmos_headers(res);
  res.status = 404;
  res.set_content(
      R"({"code": 404, "error": "Not Found", "debug": ""})",
      "application/json");
}

// IS-04 Query API helpers
static bool has_rql(const NmosReq& req) {
  return !req.get_param_value("query.rql").empty() ||
         !req.get_param_value("query.ancestry_id").empty();
}

static bool json_field_matches(const std::string& json,
                               const std::string& key,
                               const std::string& value) {
  return json.find("\"" + key + "\": \"" + value + "\"") != std::string::npos;
}

static void query_ok(NmosRes& res, const std::string& body, size_t count) {
  res.set_header("X-Paging-Limit", std::to_string(count));
  res.set_header("X-Paging-Since", "0:0");
  res.set_header("X-Paging-Until", make_version());
  nmos_ok(res, body);
}

// conn_not_found: 404 without CORS for IS-05 (clients that read the response body)
static void conn_not_found(NmosRes& res) { nmos_not_found(res); }

// ---------------------------------------------------------------------------
// DNS-SD registry discovery helpers
// ---------------------------------------------------------------------------

std::string NmosManager::effective_registry_address() const {
  if (config_->get_nmos_registry_auto_discover()) {
    std::lock_guard<std::mutex> lock(registry_disc_mutex_);
    if (!discovered_registry_address_.empty())
      return discovered_registry_address_;
  }
  return config_->get_nmos_registry_address();
}

uint16_t NmosManager::effective_registry_port() const {
  if (config_->get_nmos_registry_auto_discover()) {
    std::lock_guard<std::mutex> lock(registry_disc_mutex_);
    if (!discovered_registry_address_.empty())
      return discovered_registry_port_;
  }
  return config_->get_nmos_registry_port();
}

#ifdef _USE_AVAHI_
void NmosManager::registry_client_callback(AvahiClient* client,
                                            AvahiClientState state,
                                            void* userdata) {
  NmosManager& mgr = *reinterpret_cast<NmosManager*>(userdata);
  switch (state) {
    case AVAHI_CLIENT_S_RUNNING:
    case AVAHI_CLIENT_S_REGISTERING:
    case AVAHI_CLIENT_S_COLLISION:
      mgr.registry_browser_.reset(avahi_service_browser_new(
          client, AVAHI_IF_UNSPEC, AVAHI_PROTO_INET,
          "_nmos-register._tcp", nullptr, (AvahiLookupFlags)0,
          registry_browse_callback, &mgr));
      if (!mgr.registry_browser_) {
        BOOST_LOG_TRIVIAL(error)
            << "NmosManager:: failed to create registry browser: "
            << avahi_strerror(avahi_client_errno(client));
      }
      break;
    case AVAHI_CLIENT_FAILURE:
      BOOST_LOG_TRIVIAL(error)
          << "NmosManager:: Avahi client failure: "
          << avahi_strerror(avahi_client_errno(client));
      break;
    default:
      break;
  }
}

void NmosManager::registry_browse_callback(AvahiServiceBrowser* b,
                                            AvahiIfIndex interface,
                                            AvahiProtocol protocol,
                                            AvahiBrowserEvent event,
                                            const char* name,
                                            const char* type,
                                            const char* domain,
                                            AvahiLookupResultFlags /*flags*/,
                                            void* userdata) {
  NmosManager& mgr = *reinterpret_cast<NmosManager*>(userdata);
  switch (event) {
    case AVAHI_BROWSER_NEW:
      BOOST_LOG_TRIVIAL(info)
          << "NmosManager:: DNS-SD found NMOS registry: " << name;
      avahi_service_resolver_new(avahi_service_browser_get_client(b),
          interface, protocol, name, type, domain,
          AVAHI_PROTO_UNSPEC, (AvahiLookupFlags)0,
          registry_resolve_callback, &mgr);
      break;
    case AVAHI_BROWSER_REMOVE:
      BOOST_LOG_TRIVIAL(info)
          << "NmosManager:: DNS-SD NMOS registry removed: " << name;
      {
        std::lock_guard<std::mutex> lock(mgr.registry_disc_mutex_);
        mgr.discovered_registry_address_.clear();
        mgr.discovered_registry_port_ = 0;
      }
      {
        std::unique_lock<std::mutex> lock(mgr.events_mutex_);
        mgr.pending_events_.push({EventType::RegistryLost, 0});
      }
      mgr.events_cv_.notify_one();
      break;
    case AVAHI_BROWSER_FAILURE:
      BOOST_LOG_TRIVIAL(error)
          << "NmosManager:: Avahi browser failure: "
          << avahi_strerror(avahi_client_errno(
                 avahi_service_browser_get_client(b)));
      break;
    default:
      break;
  }
}

void NmosManager::registry_resolve_callback(AvahiServiceResolver* r,
                                             AvahiIfIndex /*iface*/,
                                             AvahiProtocol /*proto*/,
                                             AvahiResolverEvent event,
                                             const char* name,
                                             const char* /*type*/,
                                             const char* /*domain*/,
                                             const char* /*host*/,
                                             const AvahiAddress* address,
                                             uint16_t port,
                                             AvahiStringList* /*txt*/,
                                             AvahiLookupResultFlags /*flags*/,
                                             void* userdata) {
  NmosManager& mgr = *reinterpret_cast<NmosManager*>(userdata);
  if (event == AVAHI_RESOLVER_FOUND) {
    char addr[AVAHI_ADDRESS_STR_MAX];
    avahi_address_snprint(addr, sizeof(addr), address);
    BOOST_LOG_TRIVIAL(info)
        << "NmosManager:: DNS-SD resolved NMOS registry \"" << name
        << "\" at " << addr << ":" << port;
    {
      std::lock_guard<std::mutex> lock(mgr.registry_disc_mutex_);
      mgr.discovered_registry_address_ = addr;
      mgr.discovered_registry_port_    = port;
    }
    {
      std::unique_lock<std::mutex> lock(mgr.events_mutex_);
      mgr.pending_events_.push({EventType::RegistryUpdated, 0});
    }
    mgr.events_cv_.notify_one();
  } else {
    BOOST_LOG_TRIVIAL(warning)
        << "NmosManager:: DNS-SD failed to resolve NMOS registry \"" << name << "\"";
  }
  avahi_service_resolver_free(r);
}

void NmosManager::start_registry_discovery() {
  registry_poll_.reset(avahi_threaded_poll_new());
  if (!registry_poll_) {
    BOOST_LOG_TRIVIAL(error)
        << "NmosManager:: failed to create Avahi poll for registry discovery";
    return;
  }
  int error;
  registry_avahi_client_.reset(avahi_client_new(
      avahi_threaded_poll_get(registry_poll_.get()),
      AVAHI_CLIENT_NO_FAIL, registry_client_callback, this, &error));
  if (!registry_avahi_client_) {
    BOOST_LOG_TRIVIAL(error)
        << "NmosManager:: failed to create Avahi client: "
        << avahi_strerror(error);
    registry_poll_.reset();
    return;
  }
  avahi_threaded_poll_start(registry_poll_.get());
  BOOST_LOG_TRIVIAL(info) << "NmosManager:: DNS-SD registry discovery started";
}

void NmosManager::stop_registry_discovery() {
  if (registry_poll_) {
    avahi_threaded_poll_stop(registry_poll_.get());
    registry_browser_.reset();
    registry_avahi_client_.reset();
    registry_poll_.reset();
  }
}
#endif  // _USE_AVAHI_

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

std::shared_ptr<NmosManager> NmosManager::create(
    std::shared_ptr<SessionManager> session_manager,
    std::shared_ptr<Config> config) {
  return std::shared_ptr<NmosManager>(
      new NmosManager(std::move(session_manager), std::move(config)));
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

bool NmosManager::init() {
  BOOST_LOG_TRIVIAL(info) << "NmosManager:: initializing";
  std::string node_id_str = config_->get_node_id();

  // Convert node_id string to a UUID using DNS namespace (RFC 4122)
  node_id_ = make_uuid5("6ba7b810-9dad-11d1-80b4-00c04fd430c8", node_id_str);
  BOOST_LOG_TRIVIAL(info) << "NmosManager:: node_id = " << node_id_;

  device_id_ = make_uuid5(node_id_, "device");

  {
    std::unique_lock lock(resources_mutex_);
    rebuild_device_json_locked();
  }
  node_json_ = build_node_json();

  // Register session-manager observers
  session_manager_->add_source_observer(
      SessionManager::SourceObserverType::add_source,
      [this](uint8_t id, const std::string& name, const std::string& sdp) {
        return on_source_added(id, name, sdp);
      });
  session_manager_->add_source_observer(
      SessionManager::SourceObserverType::remove_source,
      [this](uint8_t id, const std::string& name, const std::string& sdp) {
        return on_source_removed(id, name, sdp);
      });
  session_manager_->add_sink_observer(
      SessionManager::SinkObserverType::add_sink,
      [this](uint8_t id, const std::string& name) {
        return on_sink_added(id, name);
      });
  session_manager_->add_sink_observer(
      SessionManager::SinkObserverType::remove_sink,
      [this](uint8_t id, const std::string& name) {
        return on_sink_removed(id, name);
      });

  running_ = true;
  setup_node_api();
  setup_connection_api();
  setup_query_api();
  if (config_->get_is08_enabled()) setup_is08_api();

  // Populate IS-04/IS-05 local state from existing session_manager snapshot
  // synchronously so the Node API serves correct responses from the first request,
  // independent of registry connectivity.
  for (const auto& src : session_manager_->get_sources())
    register_source_local(src.id);
  for (const auto& sink : session_manager_->get_sinks())
    register_sink_local(sink.id);

  if (!config_->get_interface_name(1).empty()) {
    auto [unused_ip, ip_str] = get_interface_ip(config_->get_interface_name(1));
    sec_interface_ip_str_ = ip_str;
    BOOST_LOG_TRIVIAL(info) << "NmosManager:: secondary interface IP = " << ip_str;

    auto [unused_mac, mac_str] = get_interface_mac(config_->get_interface_name(1));
    sec_interface_mac_str_ = mac_str;
    BOOST_LOG_TRIVIAL(info) << "NmosManager:: secondary interface MAC = " << mac_str;
  }

  BOOST_LOG_TRIVIAL(info) << "NmosManager:: starting async server thread";
  svr_res_ = std::async(std::launch::async, &NmosManager::server_worker, this);
  BOOST_LOG_TRIVIAL(info) << "NmosManager:: starting async registration thread";
  reg_res_ = std::async(std::launch::async, &NmosManager::registration_worker, this);
  if (config_->get_is12_enabled()) {
    BOOST_LOG_TRIVIAL(info) << "NmosManager:: starting async IS-12 notify thread";
    is12_notify_res_ = std::async(std::launch::async, &NmosManager::is12_notify_worker, this);
  }
#ifdef _USE_AVAHI_
  if (config_->get_nmos_registry_auto_discover())
    start_registry_discovery();
#endif
  BOOST_LOG_TRIVIAL(info) << "NmosManager::init() complete";
  return true;
}

bool NmosManager::terminate() {
  running_ = false;
  events_cv_.notify_all();
  if (svr_res_.valid()) svr_res_.get();
  if (reg_res_.valid()) reg_res_.get();
  if (is12_notify_res_.valid()) is12_notify_res_.get();
#ifdef _USE_AVAHI_
  stop_registry_discovery();
#endif
  return true;
}

// ---------------------------------------------------------------------------
// UUID helpers
// ---------------------------------------------------------------------------

std::string NmosManager::make_resource_uuid(const std::string& type,
                                             uint8_t id) const {
  return make_uuid5(node_id_, type + "-" + std::to_string(id));
}

// ---------------------------------------------------------------------------
// ptp-clock-manager integration
// ---------------------------------------------------------------------------

NmosManager::PtpSyncInfo NmosManager::get_ptp_clock_manager_sync() const {
  PtpSyncInfo info;
  PtpClockShm shm;
  if (!read_ptp_clock_shm_fresh(shm)) return info;  // not running / stale

  info.available = true;
  info.locked = (shm.lock_state == PTP_LOCK_LOCKED);
  info.locking = (shm.lock_state == PTP_LOCK_LOCKING);
  info.gmid_dash = eui64_to_dash_hex(shm.gmid);
  info.offset_ns = shm.offset_ns;
  info.freq_ppb = shm.freq_ppb;
  return info;
}

// ---------------------------------------------------------------------------
// JSON builders
// ---------------------------------------------------------------------------

std::string NmosManager::build_node_json() const {
  // Prefer ptp-clock-manager's discipline state when it's running: it
  // reflects whether the local system clock has actually converged on the
  // grandmaster, not just whether the driver is receiving PTP messages.
  PtpSyncInfo pcm = get_ptp_clock_manager_sync();
  bool ptp_locked;
  std::string gmid;
  if (pcm.available) {
    ptp_locked = pcm.locked;
    gmid = ptp_locked ? pcm.gmid_dash : "00-00-00-00-00-00-00-00";
  } else {
    PTPStatus ptp;
    session_manager_->get_ptp_status(ptp);
    ptp_locked = (ptp.status == "locked");
    // IS-04 gmid is "xx-xx-xx-xx-xx-xx-xx-xx"; ptp.gmid may use colons — normalise.
    gmid = ptp_locked ? colon_to_dash_mac(ptp.gmid) : "00-00-00-00-00-00-00-00";
  }

  std::ostringstream ss;
  ss << "{"
     << "\n  \"id\": \"" << node_id_ << "\""
     << ",\n  \"version\": \"" << make_version() << "\""
     << ",\n  \"label\": \"" << config_->get_nmos_label() << "\""
     << ",\n  \"description\": \"AES67 Linux Daemon\""
     << ",\n  \"tags\": {}"
     << ",\n  \"href\": \"http://" << config_->get_ip_addr_str()
     << ":" << config_->get_nmos_node_port() << "/\""
     << ",\n  \"hostname\": \"" << get_system_hostname() << "\""
     << ",\n  \"api\": {"
     << "\n    \"versions\": [\"v1.3\"],"
     << "\n    \"endpoints\": [{"
     << "\n      \"host\": \"" << config_->get_ip_addr_str() << "\","
     << "\n      \"port\": " << config_->get_nmos_node_port() << ","
     << "\n      \"protocol\": \"http\","
     << "\n      \"authorization\": false"
     << "\n    }]"
     << "\n  }"
     << ",\n  \"services\": []"
     << ",\n  \"caps\": {}"
     << ",\n  \"clocks\": [{"
     << "\n    \"name\": \"clk0\","
     << "\n    \"ref_type\": \"ptp\","
     << "\n    \"traceable\": " << (ptp_locked ? "true" : "false") << ","
     << "\n    \"version\": \"IEEE1588-2008\","
     << "\n    \"gmid\": \"" << gmid << "\","
     << "\n    \"locked\": " << (ptp_locked ? "true" : "false")
     << "\n  }]"
     << ",\n  \"interfaces\": [";
  {
    // One entry per actually-configured physical interface (SMPTE 2022-7
    // Red/Blue when a secondary is configured) — config_->get_interface_name()
    // with no index is the raw comma-joined string and was wrongly used here
    // directly as a single interface's "name".
    std::string mac0 = colon_to_dash_mac(config_->get_mac_addr_str());
    ss << "{\"name\": \"" << config_->get_interface_name(0) << "\""
       << ", \"port_id\": \"" << mac0 << "\""
       << ", \"chassis_id\": \"" << mac0 << "\"}";
    if (!config_->get_interface_name(1).empty()) {
      std::string mac1 = colon_to_dash_mac(sec_interface_mac_str_);
      ss << ", {\"name\": \"" << config_->get_interface_name(1) << "\""
         << ", \"port_id\": \"" << mac1 << "\""
         << ", \"chassis_id\": \"" << mac1 << "\"}";
    }
  }
  ss << "]"
     << "\n}";
  return ss.str();
}

void NmosManager::rebuild_device_json_locked() {
  std::ostringstream ss;
  ss << "{"
     << "\n  \"id\": \"" << device_id_ << "\""
     << ",\n  \"version\": \"" << make_version() << "\""
     << ",\n  \"label\": \"" << config_->get_nmos_label() << " Device\""
     << ",\n  \"description\": \"\""
     << ",\n  \"tags\": {}"
     << ",\n  \"type\": \"urn:x-nmos:device:generic\""
     << ",\n  \"node_id\": \"" << node_id_ << "\""
     << ",\n  \"senders\": [";
  bool first = true;
  for (const auto& [id, sr] : senders_) {
    if (!first) ss << ", ";
    ss << "\"" << sr.sender_id << "\"";
    first = false;
  }
  ss << "]"
     << ",\n  \"receivers\": [";
  first = true;
  for (const auto& [id, rr] : receivers_) {
    if (!first) ss << ", ";
    ss << "\"" << rr.receiver_id << "\"";
    first = false;
  }
  std::string base = "http://" + config_->get_ip_addr_str() + ":" +
                     std::to_string(config_->get_nmos_node_port());
  std::string ws_base = "ws://" + config_->get_ip_addr_str() + ":" +
                        std::to_string(config_->get_nmos_node_port());
  ss << "]"
     << ",\n  \"controls\": ["
     << "\n    {\"href\": \"" << base << "/x-nmos/connection/v1.1/\","
     << " \"type\": \"urn:x-nmos:control:sr-ctrl/v1.1\","
     << " \"authorization\": false},"
     << "\n    {\"href\": \"" << base << "/x-manifest/\","
     << " \"type\": \"urn:x-nmos:control:manifest-base/v1.0\","
     << " \"authorization\": false}";
  if (config_->get_is12_enabled()) {
    ss << ",\n    {\"href\": \"" << ws_base << "/x-nmos/ncp/v1.0/\","
       << " \"type\": \"urn:x-nmos:control:ncp/v1.0\","
       << " \"authorization\": false}";
  }
  if (config_->get_is08_enabled()) {
    ss << ",\n    {\"href\": \"" << base << "/x-nmos/channelmapping/v1.0/\","
       << " \"type\": \"urn:x-nmos:control:cm-ctrl/v1.0\","
       << " \"authorization\": false}";
  }
  ss << "\n  ]"
     << "\n}";
  device_json_ = ss.str();
}

std::string NmosManager::build_source_json(const StreamSource& src,
                                            const std::string& source_id) const {
  uint32_t sample_rate = config_->get_sample_rate();
  uint32_t pkt = src.max_samples_per_packet > 0 ? src.max_samples_per_packet : 48;
  std::ostringstream ss;
  ss << "{"
     << "\n  \"id\": \"" << source_id << "\""
     << ",\n  \"version\": \"" << make_version() << "\""
     << ",\n  \"label\": \"" << make_nmos_label(src.map) << "\""
     << ",\n  \"description\": \"\""
     << ",\n  \"tags\": {}"
     << ",\n  \"device_id\": \"" << device_id_ << "\""
     << ",\n  \"parents\": []"
     << ",\n  \"clock_name\": \"clk0\""
     << ",\n  \"grain_rate\": {\"numerator\": " << sample_rate
     << ", \"denominator\": " << pkt << "}"
     << ",\n  \"caps\": {}"
     << ",\n  \"format\": \"urn:x-nmos:format:audio\""
     << ",\n  \"channels\": " << make_channels_json(src.map)
     << "\n}";
  return ss.str();
}

std::string NmosManager::build_flow_json(const StreamSource& src,
                                          const std::string& source_id,
                                          const std::string& flow_id) const {
  uint32_t sample_rate = config_->get_sample_rate();
  uint32_t pkt = src.max_samples_per_packet > 0 ? src.max_samples_per_packet : 48;
  std::string media_type;
  int bit_depth;
  codec_to_nmos(src.codec, media_type, bit_depth);
  std::ostringstream ss;
  ss << "{"
     << "\n  \"id\": \"" << flow_id << "\""
     << ",\n  \"version\": \"" << make_version() << "\""
     << ",\n  \"label\": \"" << make_nmos_label(src.map) << "\""
     << ",\n  \"description\": \"\""
     << ",\n  \"tags\": {}"
     << ",\n  \"grain_rate\": {\"numerator\": " << sample_rate
     << ", \"denominator\": " << pkt << "}"
     << ",\n  \"source_id\": \"" << source_id << "\""
     << ",\n  \"parents\": []"
     << ",\n  \"device_id\": \"" << device_id_ << "\""
     << ",\n  \"format\": \"urn:x-nmos:format:audio\""
     << ",\n  \"media_type\": \"" << media_type << "\""
     << ",\n  \"sample_rate\": {\"numerator\": " << sample_rate
     << ", \"denominator\": 1}"
     << ",\n  \"bit_depth\": " << bit_depth
     << ",\n  \"channels\": " << make_channels_json(src.map)
     << "\n}";
  return ss.str();
}

std::string NmosManager::build_sender_json(const StreamSource& src,
                                            uint8_t daemon_id,
                                            const std::string& flow_id,
                                            const std::string& sender_id,
                                            const std::string& active_receiver_id) const {
  bool mcast = is_multicast(src.address);
  std::string manifest = "http://" + config_->get_ip_addr_str() + ":" +
                         std::to_string(config_->get_nmos_node_port()) +
                         "/x-manifest/senders/" + sender_id + "/manifest";
  std::ostringstream ss;
  ss << "{"
     << "\n  \"id\": \"" << sender_id << "\""
     << ",\n  \"version\": \"" << make_version() << "\""
     << ",\n  \"label\": \"" << make_nmos_label(src.map) << "\""
     << ",\n  \"description\": \"\""
     << ",\n  \"tags\": {}"
     << ",\n  \"flow_id\": \"" << flow_id << "\""
     << ",\n  \"transport\": \""
     << (mcast ? "urn:x-nmos:transport:rtp.mcast"
               : "urn:x-nmos:transport:rtp.ucast")
     << "\""
     << ",\n  \"device_id\": \"" << device_id_ << "\""
     << ",\n  \"manifest_href\": \"" << manifest << "\""
     << ",\n  \"interface_bindings\": [\"" << config_->get_interface_name(0) << "\"";
  if (is_dual_leg()) ss << ", \"" << config_->get_interface_name(1) << "\"";
  ss << "]"
     << ",\n  \"subscription\": {\"receiver_id\": ";
  if (active_receiver_id.empty()) ss << "null";
  else ss << "\"" << active_receiver_id << "\"";
  ss << ", \"active\": " << std::boolalpha << src.enabled << "}"
     << "\n}";
  return ss.str();
}

std::string NmosManager::build_receiver_json(const StreamSink& sink,
                                              const std::string& receiver_id,
                                              const std::string& active_sender_id) const {
  std::ostringstream ss;
  ss << "{"
     << "\n  \"id\": \"" << receiver_id << "\""
     << ",\n  \"version\": \"" << make_version() << "\""
     << ",\n  \"label\": \"" << make_nmos_label(sink.map) << "\""
     << ",\n  \"description\": \"\""
     << ",\n  \"tags\": {}"
     << ",\n  \"device_id\": \"" << device_id_ << "\""
     << ",\n  \"transport\": \"urn:x-nmos:transport:rtp.mcast\""
     << ",\n  \"interface_bindings\": [\"" << config_->get_interface_name(0) << "\"";
  if (is_dual_leg()) ss << ", \"" << config_->get_interface_name(1) << "\"";
  ss << "]"
     << ",\n  \"format\": \"urn:x-nmos:format:audio\"";

  // Caps derived from live config — always reflects current settings
  uint32_t sr = config_->get_sample_rate();
  size_t ch = sink.map.size() > 0 ? sink.map.size() : 8;
  uint32_t max_samples = config_->get_max_tic_frame_size();
  // Enumerate valid AES67 packet times: 0.125, 0.25, 1, 4 ms
  // A ptime is valid when it yields a whole number of samples ≤ max_tic_frame_size
  static constexpr double aes67_ptimes_ms[] = {0.125, 0.25, 1.0, 4.0};
  std::ostringstream pt;
  bool first_pt = true;
  for (double p : aes67_ptimes_ms) {
    double samples = p * sr / 1000.0;
    if (samples == static_cast<uint32_t>(samples) && static_cast<uint32_t>(samples) <= max_samples) {
      if (!first_pt) pt << ", ";
      pt << p;
      first_pt = false;
    }
  }
  ss << ",\n  \"caps\": {"
     << "\n    \"media_types\": [\"audio/L24\", \"audio/L16\", \"audio/AM824\"],"
     << "\n    \"constraint_sets\": [{"
     << "\n      \"urn:x-nmos:cap:format:channel_count\": {\"minimum\": 1, \"maximum\": " << ch << "},"
     << "\n      \"urn:x-nmos:cap:format:sample_depth\": {\"enum\": [16, 24, 32]},"
     << "\n      \"urn:x-nmos:cap:format:sample_rate\": {\"enum\": [{\"denominator\": 1, \"numerator\": " << sr << "}]},"
     << "\n      \"urn:x-nmos:cap:transport:packet_time\": {\"enum\": [" << pt.str() << "]}"
     << "\n    }]"
     << "\n  }"
     << ",\n  \"subscription\": {\"sender_id\": ";
  if (active_sender_id.empty()) ss << "null";
  else ss << "\"" << active_sender_id << "\"";
  ss << ", \"active\": " << std::boolalpha << !active_sender_id.empty() << "}"
     << "\n}";
  return ss.str();
}

std::string NmosManager::build_receiver_json(const ReceiverResources& rr) const {
  return build_receiver_json(rr.sink, rr.receiver_id, rr.active_sender_id);
}

// ---------------------------------------------------------------------------
// IS-04 WebSocket subscription helpers
// ---------------------------------------------------------------------------

std::string NmosManager::subscription_json(const Subscription& sub) const {
  std::string ws_href =
      "ws://" + config_->get_ip_addr_str() + ":" +
      std::to_string(config_->get_nmos_node_port()) +
      "/x-nmos/query/v1.3/subscriptions/" + sub.id;
  std::ostringstream ss;
  ss << "{"
     << "\"id\": \"" << sub.id << "\","
     << "\"resource_path\": \"" << sub.resource_path << "\","
     << "\"params\": {},"
     << "\"persist\": " << std::boolalpha << sub.persist << ","
     << "\"secure\": false,"
     << "\"authorization\": false,"
     << "\"max_update_rate_ms\": 100,"
     << "\"ws_href\": \"" << ws_href << "\""
     << "}";
  return ss.str();
}

std::string NmosManager::build_initial_grain(const std::string& resource_path,
                                              const std::string& grain_source_id,
                                              const std::string& grain_flow_id) const {
  std::string ts = make_tai_timestamp();
  std::ostringstream data;
  data << "[";
  bool first = true;

  if (resource_path == "/nodes") {
    data << "{\"path\": \"/" << node_id_ << "\", \"post\": " << build_node_json() << "}";
    first = false;
  } else if (resource_path == "/devices") {
    std::shared_lock lock(resources_mutex_);
    if (!device_json_.empty()) {
      data << "{\"path\": \"/" << device_id_ << "\", \"post\": " << device_json_ << "}";
      first = false;
    }
  } else if (resource_path == "/sources") {
    std::shared_lock lock(resources_mutex_);
    for (const auto& [id, sr] : senders_) {
      if (!first) data << ", ";
      data << "{\"path\": \"/" << sr.source_id << "\", \"post\": " << sr.source_json << "}";
      first = false;
    }
  } else if (resource_path == "/flows") {
    std::shared_lock lock(resources_mutex_);
    for (const auto& [id, sr] : senders_) {
      if (!first) data << ", ";
      data << "{\"path\": \"/" << sr.flow_id << "\", \"post\": " << sr.flow_json << "}";
      first = false;
    }
  } else if (resource_path == "/senders") {
    std::shared_lock lock(resources_mutex_);
    for (const auto& [id, sr] : senders_) {
      if (!first) data << ", ";
      data << "{\"path\": \"/" << sr.sender_id << "\", \"post\": " << sr.sender_json << "}";
      first = false;
    }
  } else if (resource_path == "/receivers") {
    std::shared_lock lock(resources_mutex_);
    for (const auto& [id, rr] : receivers_) {
      if (!first) data << ", ";
      data << "{\"path\": \"/" << rr.receiver_id
           << "\", \"post\": " << build_receiver_json(rr) << "}";
      first = false;
    }
  }
  (void)first;
  data << "]";

  std::ostringstream ss;
  ss << "{"
     << "\"grain_type\": \"event\","
     << "\"source_id\": \"" << grain_source_id << "\","
     << "\"flow_id\": \"" << grain_flow_id << "\","
     << "\"origin_timestamp\": \"" << ts << "\","
     << "\"sync_timestamp\": \"" << ts << "\","
     << "\"creation_timestamp\": \"" << ts << "\","
     << "\"rate\": {\"numerator\": 0, \"denominator\": 1},"
     << "\"duration\": {\"numerator\": 0, \"denominator\": 1},"
     << "\"grain\": {"
     << "\"type\": \"urn:x-nmos:format:data.event\","
     << "\"topic\": \"" << resource_path << "/\","
     << "\"data\": " << data.str()
     << "}"
     << "}";
  return ss.str();
}

// ---------------------------------------------------------------------------
// Beast HTTP + WebSocket server — single port for both protocols
// ---------------------------------------------------------------------------

void NmosManager::setup_node_api() {
  // CORS preflight
  nmos_options(R"(.*)", [](const NmosReq&, NmosRes& res) {
    set_nmos_headers(res);
    res.status = 200;
  });

  // Base discovery paths
  nmos_get("/x-nmos/", [](const NmosReq&, NmosRes& res) {
    nmos_ok(res, "[\"node/\", \"connection/\", \"query/\"]");
  });
  nmos_get("/x-nmos/node/", [](const NmosReq&, NmosRes& res) {
    nmos_ok(res, "[\"v1.3/\"]");
  });
  nmos_get("/x-nmos/node/v1.3/", [](const NmosReq&, NmosRes& res) {
    nmos_ok(res, "[\"self/\", \"devices/\", \"sources/\", \"flows/\", \"senders/\", \"receivers/\"]");
  });

  // Self — always rebuild to reflect live PTP clock status
  nmos_get(R"(/x-nmos/node/v1\.3/self/?)", [this](const NmosReq&, NmosRes& res) {
    nmos_ok(res, build_node_json());
  });

  // Devices - list
  nmos_get("/x-nmos/node/v1.3/devices/", [this](const NmosReq&, NmosRes& res) {
    std::shared_lock lock(resources_mutex_);
    nmos_ok(res, "[" + device_json_ + "]");
  });
  // Devices - single
  nmos_get(R"(/x-nmos/node/v1.3/devices/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::string id = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      if (id == device_id_) {
        nmos_ok(res, device_json_);
      } else {
        nmos_not_found(res);
      }
    });

  // Sources - list
  nmos_get("/x-nmos/node/v1.3/sources/", [this](const NmosReq&, NmosRes& res) {
    std::shared_lock lock(resources_mutex_);
    std::ostringstream ss;
    ss << "[";
    bool first = true;
    for (const auto& [id, sr] : senders_) {
      if (!first) ss << ", ";
      ss << sr.source_json;
      first = false;
    }
    ss << "]";
    nmos_ok(res, ss.str());
  });
  // Sources - single
  nmos_get(R"(/x-nmos/node/v1.3/sources/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, sr] : senders_) {
        if (sr.source_id == uuid) {
          nmos_ok(res, sr.source_json);
          return;
        }
      }
      nmos_not_found(res);
    });

  // Flows - list
  nmos_get("/x-nmos/node/v1.3/flows/", [this](const NmosReq&, NmosRes& res) {
    std::shared_lock lock(resources_mutex_);
    std::ostringstream ss;
    ss << "[";
    bool first = true;
    for (const auto& [id, sr] : senders_) {
      if (!first) ss << ", ";
      ss << sr.flow_json;
      first = false;
    }
    ss << "]";
    nmos_ok(res, ss.str());
  });
  // Flows - single
  nmos_get(R"(/x-nmos/node/v1.3/flows/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, sr] : senders_) {
        if (sr.flow_id == uuid) {
          nmos_ok(res, sr.flow_json);
          return;
        }
      }
      nmos_not_found(res);
    });

  // Senders - SDP (must be registered before the single-sender handler)
  nmos_get(R"(/x-nmos/node/v1.3/senders/([^/]+)/sdp)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string sender_uuid = req.matches[1];
      uint8_t daemon_id = 0;
      bool found = false;
      {
        std::shared_lock lock(resources_mutex_);
        for (const auto& [id, sr] : senders_) {
          if (sr.sender_id == sender_uuid) {
            daemon_id = id;
            found = true;
            break;
          }
        }
      }
      if (!found) {
        nmos_not_found(res);
        return;
      }
      std::string sdp;
      if (auto ec = session_manager_->get_source_sdp(daemon_id, sdp); !ec) {
        set_nmos_headers(res);
        res.set_content(sdp, "application/sdp");
      } else {
        nmos_not_found(res);
      }
    });
  // Senders - list
  nmos_get("/x-nmos/node/v1.3/senders/", [this](const NmosReq&, NmosRes& res) {
    std::shared_lock lock(resources_mutex_);
    std::ostringstream ss;
    ss << "[";
    bool first = true;
    for (const auto& [id, sr] : senders_) {
      if (!first) ss << ", ";
      ss << sr.sender_json;
      first = false;
    }
    ss << "]";
    nmos_ok(res, ss.str());
  });
  // Senders - single
  nmos_get(R"(/x-nmos/node/v1.3/senders/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, sr] : senders_) {
        if (sr.sender_id == uuid) {
          nmos_ok(res, sr.sender_json);
          return;
        }
      }
      nmos_not_found(res);
    });

  // Receivers - list
  nmos_get("/x-nmos/node/v1.3/receivers/", [this](const NmosReq&, NmosRes& res) {
    std::shared_lock lock(resources_mutex_);
    std::ostringstream ss;
    ss << "[";
    bool first = true;
    for (const auto& [id, rr] : receivers_) {
      if (!first) ss << ", ";
      ss << build_receiver_json(rr);
      first = false;
    }
    ss << "]";
    nmos_ok(res, ss.str());
  });
  // Receivers - single
  nmos_get(R"(/x-nmos/node/v1.3/receivers/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, rr] : receivers_) {
        if (rr.receiver_id == uuid) {
          nmos_ok(res, build_receiver_json(rr));
          return;
        }
      }
      nmos_not_found(res);
    });

  // IS-04 manifest-base/v1.0 — SDP manifest for each sender
  nmos_get(R"(/x-manifest/senders/([^/]+)/manifest)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      uint8_t daemon_id = 0;
      bool found = false;
      {
        std::shared_lock lock(resources_mutex_);
        for (const auto& [id, sr] : senders_) {
          if (sr.sender_id == uuid) { daemon_id = id; found = true; break; }
        }
      }
      if (!found) { nmos_not_found(res); return; }
      std::string sdp;
      if (!session_manager_->get_source_sdp(daemon_id, sdp)) {
        res.set_header("Access-Control-Allow-Origin", "*");
        res.set_content(sdp, "application/sdp");
      } else {
        nmos_not_found(res);
      }
    });
}

// ---------------------------------------------------------------------------
// IS-05 Connection Management API
// ---------------------------------------------------------------------------

// --- Static SDP helpers ---

// Extract first multicast/unicast destination IP from c= line.
static bool sdp_has_dup(const std::string& sdp) {
  return sdp.find("a=group:DUP") != std::string::npos;
}

// Extract destination IP from the (leg+1)th m=audio section's c= line.
static std::string sdp_connection_ip(const std::string& sdp, int leg = 0) {
  size_t pos = 0;
  for (int i = 0; i <= leg; i++) {
    pos = sdp.find("m=audio ", pos);
    if (pos == std::string::npos) return "";
    if (i < leg) pos++;
  }
  size_t next_m = sdp.find("\nm=", pos + 1);
  size_t c = sdp.find("c=IN IP4 ", pos);
  if (c == std::string::npos || (next_m != std::string::npos && c > next_m)) return "";
  size_t start = c + 9;
  size_t end = sdp.find_first_of("/\r\n", start);
  return sdp.substr(start, end == std::string::npos ? std::string::npos : end - start);
}

// Extract port from the (leg+1)th m=audio line.
static uint16_t sdp_media_port(const std::string& sdp, int leg = 0) {
  size_t pos = 0;
  for (int i = 0; i <= leg; i++) {
    pos = sdp.find("m=audio ", pos);
    if (pos == std::string::npos) return 5004;
    if (i < leg) pos++;
  }
  pos += 8;
  auto end = sdp.find(' ', pos);
  try { return static_cast<uint16_t>(std::stoi(sdp.substr(pos, end - pos))); }
  catch (...) { return 5004; }
}

// Extract source IP from the (leg+1)th a=source-filter line (SSM).
static std::string sdp_source_filter_ip(const std::string& sdp, int leg = 0) {
  const std::string prefix = "a=source-filter: incl IN IP4 ";
  size_t pos = 0;
  for (int i = 0; i <= leg; i++) {
    pos = sdp.find(prefix, pos);
    if (pos == std::string::npos) return "";
    if (i < leg) pos += prefix.size();
  }
  pos += prefix.size();
  // format: <mcast> <source>
  auto sp = sdp.find(' ', pos);
  if (sp == std::string::npos) return "";
  auto end = sdp.find_first_of("\r\n", sp + 1);
  return sdp.substr(sp + 1, end == std::string::npos ? std::string::npos : end - sp - 1);
}

// --- Transport params JSON helpers ---

std::string NmosManager::tp_sender_json(const SenderTp& tp) const {
  std::ostringstream ss;
  ss << std::boolalpha
     << "{\"source_ip\": \"" << tp.source_ip << "\""
     << ", \"destination_ip\": \"" << tp.destination_ip << "\""
     << ", \"source_port\": " << tp.source_port
     << ", \"destination_port\": " << tp.destination_port
     << ", \"rtp_enabled\": " << tp.rtp_enabled
     << "}";
  return ss.str();
}

std::string NmosManager::tp_receiver_json(const ReceiverTp& tp) const {
  std::ostringstream ss;
  ss << std::boolalpha
     << "{\"interface_ip\": \"" << tp.interface_ip << "\""
     << ", \"multicast_ip\": ";
  if (tp.multicast_ip.empty()) ss << "null";
  else ss << "\"" << tp.multicast_ip << "\"";
  ss << ", \"destination_port\": " << tp.destination_port
     << ", \"source_ip\": \"" << tp.source_ip << "\""
     << ", \"rtp_enabled\": " << tp.rtp_enabled
     << "}";
  return ss.str();
}

std::string NmosManager::activation_json(const Is05Activation& act) const {
  auto q = [](const std::string& s) -> std::string {
    return s.empty() ? "null" : "\"" + s + "\"";
  };
  std::ostringstream ss;
  ss << "{\"mode\": " << q(act.mode)
     << ", \"requested_time\": " << q(act.requested_time)
     << ", \"activation_time\": " << q(act.activation_time)
     << "}";
  return ss.str();
}

std::string NmosManager::staged_sender_json(const SenderResources& sr) const {
  std::ostringstream ss;
  ss << std::boolalpha
     << "{\"master_enable\": " << sr.staged_master_enable
     << ", \"receiver_id\": ";
  if (sr.staged_receiver_id.empty()) ss << "null";
  else ss << "\"" << sr.staged_receiver_id << "\"";
  ss << ", \"activation\": " << activation_json(sr.staged_act)
     << ", \"transport_params\": [";
  for (size_t i = 0; i < sr.staged_tp.size(); ++i) {
    if (i) ss << ", ";
    ss << tp_sender_json(sr.staged_tp[i]);
  }
  ss << "]}";
  return ss.str();
}

std::string NmosManager::active_sender_json(const SenderResources& sr) const {
  std::ostringstream ss;
  ss << std::boolalpha
     << "{\"master_enable\": " << sr.active_master_enable
     << ", \"receiver_id\": ";
  if (sr.active_receiver_id.empty()) ss << "null";
  else ss << "\"" << sr.active_receiver_id << "\"";
  ss << ", \"activation\": " << activation_json(sr.active_act)
     << ", \"transport_params\": [";
  for (size_t i = 0; i < sr.active_tp.size(); ++i) {
    if (i) ss << ", ";
    ss << tp_sender_json(sr.active_tp[i]);
  }
  ss << "]}";
  return ss.str();
}

std::string NmosManager::staged_receiver_json(const ReceiverResources& rr) const {
  std::ostringstream ss;
  ss << std::boolalpha
     << "{\"master_enable\": " << rr.staged_master_enable
     << ", \"sender_id\": ";
  if (rr.staged_sender_id.empty()) ss << "null";
  else ss << "\"" << rr.staged_sender_id << "\"";
  ss << ", \"activation\": " << activation_json(rr.staged_act)
     << ", \"transport_params\": [";
  for (size_t i = 0; i < rr.staged_tp.size(); ++i) {
    if (i) ss << ", ";
    ss << tp_receiver_json(rr.staged_tp[i]);
  }
  ss << "]}";
  return ss.str();
}

std::string NmosManager::active_receiver_json(const ReceiverResources& rr) const {
  std::ostringstream ss;
  ss << std::boolalpha
     << "{\"master_enable\": " << rr.active_master_enable
     << ", \"sender_id\": ";
  if (rr.active_sender_id.empty()) ss << "null";
  else ss << "\"" << rr.active_sender_id << "\"";
  ss << ", \"activation\": " << activation_json(rr.active_act)
     << ", \"transport_params\": [";
  for (size_t i = 0; i < rr.active_tp.size(); ++i) {
    if (i) ss << ", ";
    ss << tp_receiver_json(rr.active_tp[i]);
  }
  ss << "]}";
  return ss.str();
}

// --- Transport param builders from daemon state ---

std::vector<NmosManager::SenderTp> NmosManager::build_sender_tp(
    const StreamSource& src) const {
  std::vector<SenderTp> tps;
  SenderTp tp0;
  tp0.source_ip       = config_->get_ip_addr_str();
  tp0.destination_ip  = src.address;
  tp0.source_port     = config_->get_rtp_port();
  tp0.destination_port = config_->get_rtp_port();
  tp0.rtp_enabled     = src.enabled;
  tps.push_back(tp0);

  if (is_dual_leg()) {
    SenderTp tp1;
    tp1.source_ip   = sec_interface_ip_str_;
    tp1.rtp_enabled = src.enabled;
    std::string sdp;
    session_manager_->get_source_sdp(src.id, sdp);
    if (sdp_has_dup(sdp)) {
      tp1.destination_ip   = sdp_connection_ip(sdp, 1);
      tp1.source_port = tp1.destination_port = sdp_media_port(sdp, 1);
    } else {
      tp1.destination_ip   = src.address;
      tp1.source_port = tp1.destination_port = config_->get_rtp_port_sec();
    }
    tps.push_back(tp1);
  }
  return tps;
}

std::vector<NmosManager::ReceiverTp> NmosManager::build_receiver_tp_from_sdp(
    const std::string& sdp) const {
  std::vector<ReceiverTp> tps;
  ReceiverTp tp0;
  tp0.interface_ip     = config_->get_ip_addr_str();
  std::string dest0    = sdp_connection_ip(sdp, 0);
  if (is_multicast(dest0)) tp0.multicast_ip = dest0;
  tp0.destination_port = sdp_media_port(sdp, 0);
  std::string src0     = sdp_source_filter_ip(sdp, 0);
  tp0.source_ip        = src0.empty() ? "auto" : src0;
  tp0.rtp_enabled      = true;
  tps.push_back(tp0);

  if (is_dual_leg()) {
    ReceiverTp tp1;
    tp1.interface_ip = sec_interface_ip_str_;
    tp1.rtp_enabled  = true;
    if (sdp_has_dup(sdp)) {
      std::string dest1 = sdp_connection_ip(sdp, 1);
      if (is_multicast(dest1)) tp1.multicast_ip = dest1;
      tp1.destination_port = sdp_media_port(sdp, 1);
      std::string src1     = sdp_source_filter_ip(sdp, 1);
      tp1.source_ip        = src1.empty() ? "auto" : src1;
    } else {
      /* Segregated 2022-7 network: primary stream not present on secondary leg */
      tp1.rtp_enabled      = false;
      tp1.source_ip        = "auto";
      tp1.destination_port = 5004;
    }
    tps.push_back(tp1);
  }
  return tps;
}

// --- PATCH body parsing ---

// Merges a partial IS-05 PATCH body into the sender's staged state.
bool NmosManager::patch_sender_staged(uint8_t daemon_id,
                                      const std::string& body,
                                      std::string& err,
                                      std::string& staged_json_out) {
  // Parse with boost property_tree
  namespace pt_ns = boost::property_tree;
  pt_ns::ptree pt;
  try {
    std::istringstream ss(body);
    pt_ns::read_json(ss, pt);
  } catch (const std::exception& e) {
    err = e.what();
    return false;
  }

  std::unique_lock lock(resources_mutex_);
  auto it = senders_.find(daemon_id);
  if (it == senders_.end()) { err = "not found"; return false; }
  SenderResources& sr = it->second;

  if (auto v = pt.get_optional<bool>("master_enable"))
    sr.staged_master_enable = *v;
  if (auto v = pt.get_optional<std::string>("receiver_id"))
    sr.staged_receiver_id = (*v == "null") ? "" : *v;

  auto& act = sr.staged_act;
  if (auto v = pt.get_optional<std::string>("activation.mode"))
    act.mode = (*v == "null") ? "" : *v;
  if (auto v = pt.get_optional<std::string>("activation.requested_time"))
    act.requested_time = (*v == "null") ? "" : *v;

  // transport_params (array — iterate all legs up to resource leg count)
  auto tp_child = pt.get_child_optional("transport_params");
  if (tp_child) {
    int leg_idx = 0;
    for (auto it = tp_child->begin();
         it != tp_child->end() && leg_idx < (int)sr.staged_tp.size();
         ++it, ++leg_idx) {
      const auto& leg = it->second;
      if (auto v = leg.get_optional<std::string>("source_ip"))
        sr.staged_tp[leg_idx].source_ip = *v;
      if (auto v = leg.get_optional<std::string>("destination_ip"))
        sr.staged_tp[leg_idx].destination_ip = *v;
      if (auto v = leg.get_optional<uint16_t>("source_port"))
        sr.staged_tp[leg_idx].source_port = *v;
      if (auto v = leg.get_optional<uint16_t>("destination_port"))
        sr.staged_tp[leg_idx].destination_port = *v;
      if (auto v = leg.get_optional<bool>("rtp_enabled"))
        sr.staged_tp[leg_idx].rtp_enabled = *v;
    }
  }

  // Capture staged JSON BEFORE activation fires observer events
  staged_json_out = staged_sender_json(senders_.at(daemon_id));

  // Handle activation
  if (act.mode == "activate_immediate") {
    act.activation_time = make_version();
    // reset after copy to active
    lock.unlock();
    apply_sender_activation(daemon_id);
    // re-lock to reset staged activation
    lock.lock();
    if (senders_.count(daemon_id))
      senders_[daemon_id].staged_act = {};
  } else if (act.mode == "activate_scheduled_relative" ||
             act.mode == "activate_scheduled_absolute") {
    int64_t deadline = 0;
    auto now_ns = std::chrono::duration_cast<std::chrono::nanoseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count();
    if (act.mode == "activate_scheduled_relative" && !act.requested_time.empty()) {
      // requested_time = "<secs>:<nanos>"
      auto colon = act.requested_time.find(':');
      try {
        int64_t s = std::stoll(act.requested_time.substr(0, colon));
        int64_t n = colon != std::string::npos
                    ? std::stoll(act.requested_time.substr(colon + 1)) : 0;
        deadline = now_ns + s * 1'000'000'000LL + n;
      } catch (...) { deadline = now_ns + 1'000'000'000LL; }
    } else {
      // absolute: use TAI — approximate as system clock + 1s fallback
      deadline = now_ns + 1'000'000'000LL;
    }
    act.deadline_ns = deadline;
    {
      std::lock_guard<std::mutex> pa_lock(pending_act_mutex_);
      pending_activations_.push_back({true, daemon_id, deadline});
    }
  }
  return true;
}

bool NmosManager::patch_receiver_staged(uint8_t daemon_id,
                                        const std::string& body,
                                        std::string& err,
                                        std::string& staged_json_out) {
  namespace pt_ns = boost::property_tree;
  pt_ns::ptree pt;
  try {
    std::istringstream ss(body);
    pt_ns::read_json(ss, pt);
  } catch (const std::exception& e) {
    err = e.what();
    return false;
  }

  std::unique_lock lock(resources_mutex_);
  auto it = receivers_.find(daemon_id);
  if (it == receivers_.end()) { err = "not found"; return false; }
  ReceiverResources& rr = it->second;

  if (auto v = pt.get_optional<bool>("master_enable"))
    rr.staged_master_enable = *v;
  if (auto v = pt.get_optional<std::string>("sender_id"))
    rr.staged_sender_id = (*v == "null") ? "" : *v;

  auto& act = rr.staged_act;
  if (auto v = pt.get_optional<std::string>("activation.mode"))
    act.mode = (*v == "null") ? "" : *v;
  if (auto v = pt.get_optional<std::string>("activation.requested_time"))
    act.requested_time = (*v == "null") ? "" : *v;

  auto tp_child = pt.get_child_optional("transport_params");
  if (tp_child) {
    int leg_idx = 0;
    for (auto it2 = tp_child->begin();
         it2 != tp_child->end() && leg_idx < (int)rr.staged_tp.size();
         ++it2, ++leg_idx) {
      const auto& leg = it2->second;
      if (auto v = leg.get_optional<std::string>("interface_ip"))
        rr.staged_tp[leg_idx].interface_ip = *v;
      if (auto v = leg.get_optional<std::string>("multicast_ip"))
        rr.staged_tp[leg_idx].multicast_ip = (*v == "null") ? "" : *v;
      if (auto v = leg.get_optional<uint16_t>("destination_port"))
        rr.staged_tp[leg_idx].destination_port = *v;
      if (auto v = leg.get_optional<std::string>("source_ip"))
        rr.staged_tp[leg_idx].source_ip = *v;
      if (auto v = leg.get_optional<bool>("rtp_enabled"))
        rr.staged_tp[leg_idx].rtp_enabled = *v;
    }
  }

  // Capture staged JSON BEFORE activation fires observer events
  staged_json_out = staged_receiver_json(receivers_.at(daemon_id));

  if (act.mode == "activate_immediate") {
    act.activation_time = make_version();
    lock.unlock();
    apply_receiver_activation(daemon_id);
    lock.lock();
    if (receivers_.count(daemon_id))
      receivers_[daemon_id].staged_act = {};
  } else if (act.mode == "activate_scheduled_relative" ||
             act.mode == "activate_scheduled_absolute") {
    int64_t deadline = 0;
    auto now_ns = std::chrono::duration_cast<std::chrono::nanoseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count();
    if (act.mode == "activate_scheduled_relative" && !act.requested_time.empty()) {
      auto colon = act.requested_time.find(':');
      try {
        int64_t s = std::stoll(act.requested_time.substr(0, colon));
        int64_t n = colon != std::string::npos
                    ? std::stoll(act.requested_time.substr(colon + 1)) : 0;
        deadline = now_ns + s * 1'000'000'000LL + n;
      } catch (...) { deadline = now_ns + 1'000'000'000LL; }
    } else {
      deadline = now_ns + 1'000'000'000LL;
    }
    act.deadline_ns = deadline;
    {
      std::lock_guard<std::mutex> pa_lock(pending_act_mutex_);
      pending_activations_.push_back({false, daemon_id, deadline});
    }
  }
  return true;
}

// --- Remote sender SDP fetch ---

// Fetch SDP for a remote sender UUID:
//   1. Query the IS-04 query/registry API for the sender object to get manifest_href
//   2. Fetch the SDP (application/sdp) from manifest_href
// Falls back to empty sdp on any error so the caller can decide what to do.
void NmosManager::fetch_remote_sender_sdp(const std::string& sender_uuid,
                                           std::string& sdp) {
  sdp.clear();

  // Step 1: query registry query API for sender
  const std::string reg_host = effective_registry_address();
  const uint16_t    reg_port = effective_registry_port();
  std::string manifest_href;

  {
    httplib::Client cli(reg_host.c_str(), reg_port);
    cli.set_connection_timeout(3);
    cli.set_read_timeout(3);
    const std::string path = "/x-nmos/query/v1.3/senders/" + sender_uuid;
    auto res = cli.Get(path.c_str());
    if (res && res->status == 200) {
      // Extract manifest_href from the sender JSON
      // Simple string search to avoid pulling in a full JSON parser here
      const std::string key = "\"manifest_href\":\"";
      auto pos = res->body.find(key);
      if (pos != std::string::npos) {
        pos += key.size();
        auto end = res->body.find('"', pos);
        if (end != std::string::npos)
          manifest_href = res->body.substr(pos, end - pos);
      }
    } else {
      BOOST_LOG_TRIVIAL(warning)
          << "NmosManager:: IS-05 remote sender lookup: registry query failed for "
          << sender_uuid << " (registry " << reg_host << ":" << reg_port << ")";
    }
  }

  if (manifest_href.empty()) {
    BOOST_LOG_TRIVIAL(warning)
        << "NmosManager:: IS-05 remote sender lookup: no manifest_href for " << sender_uuid;
    return;
  }

  // Step 2: fetch SDP from manifest_href
  // Parse manifest_href for host/port/path
  auto trim_http = [](const std::string& url) -> std::tuple<std::string,uint16_t,std::string> {
    // Expect "http://host[:port]/path"
    auto after = url;
    if (after.substr(0, 7) == "http://") after = after.substr(7);
    auto slash = after.find('/');
    std::string host_port = slash != std::string::npos ? after.substr(0, slash) : after;
    std::string path      = slash != std::string::npos ? after.substr(slash) : "/";
    auto colon = host_port.find(':');
    std::string host  = colon != std::string::npos ? host_port.substr(0, colon) : host_port;
    uint16_t    port  = 80;
    if (colon != std::string::npos) {
      try { port = static_cast<uint16_t>(std::stoi(host_port.substr(colon + 1))); }
      catch (...) {}
    }
    return {host, port, path};
  };

  auto [mh_host, mh_port, mh_path] = trim_http(manifest_href);
  httplib::Client mcli(mh_host.c_str(), mh_port);
  mcli.set_connection_timeout(3);
  mcli.set_read_timeout(3);
  auto mres = mcli.Get(mh_path.c_str());
  if (mres && (mres->status == 200 || mres->status == 206)) {
    sdp = mres->body;
    BOOST_LOG_TRIVIAL(info)
        << "NmosManager:: IS-05 fetched SDP for remote sender " << sender_uuid
        << " from " << manifest_href;
  } else {
    BOOST_LOG_TRIVIAL(warning)
        << "NmosManager:: IS-05 failed to fetch SDP from " << manifest_href;
  }
}

// --- Activation execution ---

void NmosManager::apply_sender_activation(uint8_t daemon_id) {
  std::unique_lock lock(resources_mutex_);
  auto it = senders_.find(daemon_id);
  if (it == senders_.end()) return;
  SenderResources& sr = it->second;

  // Promote staged → active
  sr.active_master_enable  = sr.staged_master_enable;
  sr.active_receiver_id    = sr.staged_receiver_id;
  sr.active_act            = sr.staged_act;
  sr.active_tp             = sr.staged_tp;
  sr.active_act.mode       = sr.staged_act.mode;
  sr.active_act.activation_time = sr.staged_act.activation_time;

  // Update IS-04 sender JSON to reflect new subscription
  StreamSource src;
  lock.unlock();
  if (!session_manager_->get_source(daemon_id, src)) {
    std::unique_lock l2(resources_mutex_);
    if (senders_.count(daemon_id)) {
      SenderResources& sr2 = senders_[daemon_id];
      sr2.sender_json = build_sender_json(src, daemon_id, sr2.flow_id,
                                          sr2.sender_id, sr2.active_receiver_id);
    }
  }
}

void NmosManager::apply_receiver_activation(uint8_t daemon_id) {
  // Snapshot staged state
  bool        master_enable;
  std::string sender_id;
  std::vector<ReceiverTp> tp;
  std::string activation_time;
  std::string staged_act_mode;
  {
    std::shared_lock lock(resources_mutex_);
    auto it = receivers_.find(daemon_id);
    if (it == receivers_.end()) return;
    master_enable   = it->second.staged_master_enable;
    sender_id       = it->second.staged_sender_id;
    tp              = it->second.staged_tp;
    activation_time = it->second.staged_act.activation_time;
    staged_act_mode = it->second.staged_act.mode;
  }

  // Resolve SDP for remote sender connection
  std::string sdp;
  if (master_enable && !sender_id.empty()) {
    uint8_t src_daemon_id = 0;
    bool found_local = false;
    {
      std::shared_lock lock(resources_mutex_);
      for (const auto& [sid, sr] : senders_) {
        if (sr.sender_id == sender_id) { src_daemon_id = sid; found_local = true; break; }
      }
    }
    if (found_local) {
      session_manager_->get_source_sdp(src_daemon_id, sdp);
    } else {
      // Remote sender: query registry for manifest_href, then fetch SDP
      fetch_remote_sender_sdp(sender_id, sdp);
    }
    if (!sdp.empty())
      tp = build_receiver_tp_from_sdp(sdp);
  }

  // Reject non-audio SDPs before touching active state or calling add_sink.
  if (master_enable && !sdp.empty() &&
      sdp.find("m=audio") == std::string::npos) {
    BOOST_LOG_TRIVIAL(warning)
        << "NmosManager:: receiver " << +daemon_id
        << " activation rejected: SDP has no audio media section";
    return;
  }

  // Promote staged → active BEFORE calling add_sink.
  // add_sink fires remove+add observers which cause register_sink to run in the
  // registration_worker thread. We set preserved_active_sender_ids_ so that
  // register_sink can restore the IS-05 active sender across that cycle.
  {
    std::unique_lock lock(resources_mutex_);
    auto it = receivers_.find(daemon_id);
    if (it == receivers_.end()) return;
    ReceiverResources& rr = it->second;
    rr.active_master_enable = master_enable;
    rr.active_sender_id     = master_enable ? sender_id : "";
    rr.active_act.mode      = staged_act_mode;
    rr.active_act.activation_time = activation_time;
    rr.active_tp            = tp;
    // Preserve across the remove+add observer cycle triggered by add_sink below
    preserved_active_sender_ids_[daemon_id] = rr.active_sender_id;
  }

  // Apply session_manager connection (may trigger remove+add observer events)
  if (master_enable && !sdp.empty()) {
    StreamSink sink;
    if (!session_manager_->get_sink(daemon_id, sink)) {
      sink.use_sdp = true;
      sink.sdp     = sdp;
      sink.source  = "";
      session_manager_->add_sink(sink);
    }
  } else if (!master_enable) {
    StreamSink sink;
    if (!session_manager_->get_sink(daemon_id, sink)) {
      sink.use_sdp = false;
      sink.sdp     = "";
      sink.source  = "";
      session_manager_->add_sink(sink);
    }
  }

  // Keep sink up to date so receiver JSON reflects current SDP/subscription state
  StreamSink sink;
  if (!session_manager_->get_sink(daemon_id, sink)) {
    std::unique_lock lock(resources_mutex_);
    auto it = receivers_.find(daemon_id);
    if (it != receivers_.end()) {
      it->second.sink = sink;
    }
  }
}

// Process scheduled activations — called from registration_worker loop.
void NmosManager::process_scheduled_activations() {
  auto now_ns = std::chrono::duration_cast<std::chrono::nanoseconds>(
      std::chrono::steady_clock::now().time_since_epoch()).count();

  std::vector<PendingActivation> due;
  {
    std::lock_guard<std::mutex> lock(pending_act_mutex_);
    auto part = std::stable_partition(
        pending_activations_.begin(), pending_activations_.end(),
        [now_ns](const PendingActivation& pa) { return pa.deadline_ns > now_ns; });
    due.assign(part, pending_activations_.end());
    pending_activations_.erase(part, pending_activations_.end());
  }

  for (const auto& pa : due) {
    // Fill activation_time in staged before promoting
    {
      std::unique_lock lock(resources_mutex_);
      if (pa.is_sender) {
        auto it = senders_.find(pa.daemon_id);
        if (it != senders_.end())
          it->second.staged_act.activation_time = make_version();
      } else {
        auto it = receivers_.find(pa.daemon_id);
        if (it != receivers_.end())
          it->second.staged_act.activation_time = make_version();
      }
    }
    if (pa.is_sender) apply_sender_activation(pa.daemon_id);
    else              apply_receiver_activation(pa.daemon_id);
    // Reset staged activation
    {
      std::unique_lock lock(resources_mutex_);
      if (pa.is_sender) {
        auto it = senders_.find(pa.daemon_id);
        if (it != senders_.end()) it->second.staged_act = {};
      } else {
        auto it = receivers_.find(pa.daemon_id);
        if (it != receivers_.end()) it->second.staged_act = {};
      }
    }
  }
}

// --- IS-05 HTTP routes ---

static void conn_ok(NmosRes& res, const std::string& body) {
  res.set_header("Access-Control-Allow-Origin", "*");
  res.set_header("Cache-Control", "no-cache, no-store");
  res.set_content(body, "application/json");
}

static void conn_bad_request(NmosRes& res, const std::string& msg) {
  res.set_header("Access-Control-Allow-Origin", "*");
  res.status = 400;
  res.set_content("{\"code\":400,\"error\":\"Bad Request\",\"debug\":\"" + msg + "\"}",
                  "application/json");
}

void NmosManager::setup_connection_api() {
  // Discovery roots
  nmos_get("/x-nmos/connection/",
    [](const NmosReq&, NmosRes& res) {
      conn_ok(res, "[\"v1.1/\"]");
    });
  nmos_get("/x-nmos/connection/v1.1/",
    [](const NmosReq&, NmosRes& res) {
      conn_ok(res, "[\"single/\", \"bulk/\"]");
    });
  nmos_get("/x-nmos/connection/v1.1/single/",
    [](const NmosReq&, NmosRes& res) {
      conn_ok(res, "[\"senders/\", \"receivers/\"]");
    });
  nmos_get("/x-nmos/connection/v1.1/bulk/",
    [](const NmosReq&, NmosRes& res) {
      conn_ok(res, "[\"senders/\", \"receivers/\"]");
    });

  // ---- Single senders ----

  // List
  nmos_get("/x-nmos/connection/v1.1/single/senders/",
    [this](const NmosReq&, NmosRes& res) {
      std::shared_lock lock(resources_mutex_);
      std::ostringstream ss;
      ss << "[";
      bool first = true;
      for (const auto& [id, sr] : senders_) {
        if (!first) ss << ", ";
        ss << "\"" << sr.sender_id << "/\"";
        first = false;
      }
      ss << "]";
      conn_ok(res, ss.str());
    });

  // Index
  nmos_get(R"(/x-nmos/connection/v1\.1/single/senders/([^/]+)/?)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, sr] : senders_) {
        if (sr.sender_id == uuid) {
          conn_ok(res, "[\"constraints/\", \"staged/\", \"active/\", "
                       "\"transportfile/\", \"transporttype\"]");
          return;
        }
      }
      conn_not_found(res);
    });

  // Constraints
  nmos_get(R"(/x-nmos/connection/v1\.1/single/senders/([^/]+)/constraints/?)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, sr] : senders_) {
        if (sr.sender_id == uuid) {
          std::string c = "[";
          for (size_t i = 0; i < sr.staged_tp.size(); ++i) {
            if (i) c += ", ";
            c += "{}";
          }
          c += "]";
          conn_ok(res, c); return;
        }
      }
      conn_not_found(res);
    });

  // Staged GET
  nmos_get(R"(/x-nmos/connection/v1\.1/single/senders/([^/]+)/staged/?)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, sr] : senders_) {
        if (sr.sender_id == uuid) { conn_ok(res, staged_sender_json(sr)); return; }
      }
      conn_not_found(res);
    });

  // Staged PATCH
  nmos_patch(R"(/x-nmos/connection/v1\.1/single/senders/([^/]+)/staged)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      uint8_t daemon_id = 0;
      bool found = false;
      {
        std::shared_lock lock(resources_mutex_);
        for (const auto& [id, sr] : senders_) {
          if (sr.sender_id == uuid) { daemon_id = id; found = true; break; }
        }
      }
      if (!found) { conn_not_found(res); return; }
      std::string err, staged_json;
      if (!patch_sender_staged(daemon_id, req.body, err, staged_json)) {
        conn_bad_request(res, err); return;
      }
      conn_ok(res, staged_json);
    });

  // Active GET
  nmos_get(R"(/x-nmos/connection/v1\.1/single/senders/([^/]+)/active/?)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, sr] : senders_) {
        if (sr.sender_id == uuid) { conn_ok(res, active_sender_json(sr)); return; }
      }
      conn_not_found(res);
    });

  // Transport file (SDP)
  nmos_get(R"(/x-nmos/connection/v1\.1/single/senders/([^/]+)/transportfile/?)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      uint8_t daemon_id = 0;
      bool found = false, enabled = false;
      {
        std::shared_lock lock(resources_mutex_);
        for (const auto& [id, sr] : senders_) {
          if (sr.sender_id == uuid) {
            daemon_id = id; found = true;
            enabled = sr.active_master_enable; break;
          }
        }
      }
      if (!found) { conn_not_found(res); return; }
      if (!enabled) { conn_not_found(res); return; }
      std::string sdp;
      if (!session_manager_->get_source_sdp(daemon_id, sdp)) {
        res.set_header("Access-Control-Allow-Origin", "*");
        res.set_content(sdp, "application/sdp");
      } else conn_not_found(res);
    });

  // Transport type
  nmos_get(R"(/x-nmos/connection/v1\.1/single/senders/([^/]+)/transporttype)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, sr] : senders_) {
        if (sr.sender_id == uuid) {
          conn_ok(res, "\"urn:x-nmos:transport:rtp.mcast\""); return;
        }
      }
      conn_not_found(res);
    });

  // ---- Single receivers ----

  nmos_get("/x-nmos/connection/v1.1/single/receivers/",
    [this](const NmosReq&, NmosRes& res) {
      std::shared_lock lock(resources_mutex_);
      std::ostringstream ss;
      ss << "[";
      bool first = true;
      for (const auto& [id, rr] : receivers_) {
        if (!first) ss << ", ";
        ss << "\"" << rr.receiver_id << "/\"";
        first = false;
      }
      ss << "]";
      conn_ok(res, ss.str());
    });

  nmos_get(R"(/x-nmos/connection/v1\.1/single/receivers/([^/]+)/?)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, rr] : receivers_) {
        if (rr.receiver_id == uuid) {
          conn_ok(res, "[\"constraints/\", \"staged/\", \"active/\", \"transporttype\"]");
          return;
        }
      }
      conn_not_found(res);
    });

  nmos_get(R"(/x-nmos/connection/v1\.1/single/receivers/([^/]+)/constraints/?)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, rr] : receivers_) {
        if (rr.receiver_id == uuid) {
          std::string c = "[";
          for (size_t i = 0; i < rr.staged_tp.size(); ++i) {
            if (i) c += ", ";
            c += "{}";
          }
          c += "]";
          conn_ok(res, c); return;
        }
      }
      conn_not_found(res);
    });

  nmos_get(R"(/x-nmos/connection/v1\.1/single/receivers/([^/]+)/staged/?)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, rr] : receivers_) {
        if (rr.receiver_id == uuid) { conn_ok(res, staged_receiver_json(rr)); return; }
      }
      conn_not_found(res);
    });

  nmos_patch(R"(/x-nmos/connection/v1\.1/single/receivers/([^/]+)/staged)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      uint8_t daemon_id = 0;
      bool found = false;
      {
        std::shared_lock lock(resources_mutex_);
        for (const auto& [id, rr] : receivers_) {
          if (rr.receiver_id == uuid) { daemon_id = id; found = true; break; }
        }
      }
      if (!found) { conn_not_found(res); return; }
      std::string err, staged_json;
      if (!patch_receiver_staged(daemon_id, req.body, err, staged_json)) {
        conn_bad_request(res, err); return;
      }
      conn_ok(res, staged_json);
    });

  nmos_get(R"(/x-nmos/connection/v1\.1/single/receivers/([^/]+)/active/?)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, rr] : receivers_) {
        if (rr.receiver_id == uuid) { conn_ok(res, active_receiver_json(rr)); return; }
      }
      conn_not_found(res);
    });

  nmos_get(R"(/x-nmos/connection/v1\.1/single/receivers/([^/]+)/transporttype)",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, rr] : receivers_) {
        if (rr.receiver_id == uuid) {
          conn_ok(res, "\"urn:x-nmos:transport:rtp.mcast\""); return;
        }
      }
      conn_not_found(res);
    });

  // ---- Bulk endpoints ----

  nmos_get("/x-nmos/connection/v1.1/bulk/senders",
    [](const NmosReq&, NmosRes& res) {
      res.status = 405;
      res.set_header("Allow", "POST");
      res.set_content("{\"code\":405,\"error\":\"Method Not Allowed\",\"debug\":\"\"}",
                      "application/json");
    });
  nmos_get("/x-nmos/connection/v1.1/bulk/receivers",
    [](const NmosReq&, NmosRes& res) {
      res.status = 405;
      res.set_header("Allow", "POST");
      res.set_content("{\"code\":405,\"error\":\"Method Not Allowed\",\"debug\":\"\"}",
                      "application/json");
    });

  auto bulk_handler = [this](const NmosReq& req, NmosRes& res,
                              bool is_sender) {
    // Body: [{id: "uuid", params: {...}}, ...]
    namespace pt_ns = boost::property_tree;
    pt_ns::ptree pt;
    try {
      std::istringstream ss(req.body);
      pt_ns::read_json(ss, pt);
    } catch (...) {
      conn_bad_request(res, "invalid JSON"); return;
    }

    std::ostringstream out;
    out << "[";
    bool first_out = true;
    for (const auto& [key, item] : pt) {
      std::string uuid = item.get<std::string>("id", "");
      // Serialize the sub-object back to JSON for patch_*_staged
      std::ostringstream params_ss;
      try {
        pt_ns::write_json(params_ss, item.get_child("params"));
      } catch (...) { params_ss.str("{}"); }

      uint8_t daemon_id = 0;
      bool found = false;
      {
        std::shared_lock lock(resources_mutex_);
        if (is_sender) {
          for (const auto& [id, sr] : senders_) {
            if (sr.sender_id == uuid) { daemon_id = id; found = true; break; }
          }
        } else {
          for (const auto& [id, rr] : receivers_) {
            if (rr.receiver_id == uuid) { daemon_id = id; found = true; break; }
          }
        }
      }

      int code = 200;
      std::string err, staged_json_ignored;
      if (!found) { code = 404; err = "not found"; }
      else {
        bool ok = is_sender ? patch_sender_staged(daemon_id, params_ss.str(), err, staged_json_ignored)
                            : patch_receiver_staged(daemon_id, params_ss.str(), err, staged_json_ignored);
        if (!ok) code = 400;
      }

      if (!first_out) out << ", ";
      out << "{\"id\": \"" << uuid << "\", \"code\": " << code;
      if (!err.empty()) out << ", \"error\": \"" << err << "\"";
      out << "}";
      first_out = false;
    }
    out << "]";
    res.status = 200;
    conn_ok(res, out.str());
  };

  nmos_post("/x-nmos/connection/v1.1/bulk/senders",
    [bulk_handler](const NmosReq& req, NmosRes& res) {
      bulk_handler(req, res, true);
    });
  nmos_post("/x-nmos/connection/v1.1/bulk/receivers",
    [bulk_handler](const NmosReq& req, NmosRes& res) {
      bulk_handler(req, res, false);
    });
}

// ---------------------------------------------------------------------------
// IS-04 Query API v1.3
// ---------------------------------------------------------------------------

void NmosManager::setup_query_api() {
  // Discovery
  nmos_get("/x-nmos/query/",
    [](const NmosReq&, NmosRes& res) {
      nmos_ok(res, "[\"v1.3/\"]");
    });
  nmos_get("/x-nmos/query/v1.3/",
    [](const NmosReq&, NmosRes& res) {
      nmos_ok(res, "[\"nodes/\", \"devices/\", \"sources/\", \"flows/\", "
                   "\"senders/\", \"receivers/\", \"subscriptions/\"]");
    });

  // IS-04 Subscriptions
  nmos_get("/x-nmos/query/v1.3/subscriptions/",
    [this](const NmosReq&, NmosRes& res) {
      std::lock_guard lk(subscriptions_mutex_);
      std::ostringstream ss;
      ss << "[";
      bool first = true;
      for (const auto& [id, sub] : subscriptions_) {
        if (!first) ss << ", ";
        ss << subscription_json(sub);
        first = false;
      }
      ss << "]";
      nmos_ok(res, ss.str());
    });

  nmos_post("/x-nmos/query/v1.3/subscriptions/",
    [this](const NmosReq& req, NmosRes& res) {
      // Parse resource_path from body
      std::string resource_path;
      bool persist = false;
      try {
        boost::property_tree::ptree pt;
        std::istringstream is(req.body);
        boost::property_tree::read_json(is, pt);
        resource_path = pt.get<std::string>("resource_path", "");
        persist       = pt.get<bool>("persist", false);
      } catch (...) {
        set_nmos_headers(res);
        res.status = 400;
        res.set_content(R"({"code":400,"error":"Bad Request"})", "application/json");
        return;
      }
      // Validate resource_path
      static const std::set<std::string> valid_paths{
          "/nodes", "/devices", "/sources", "/flows",
          "/senders", "/receivers"};
      if (valid_paths.find(resource_path) == valid_paths.end()) {
        set_nmos_headers(res);
        res.status = 400;
        res.set_content(R"({"code":400,"error":"Invalid resource_path"})", "application/json");
        return;
      }
      Subscription sub;
      sub.id            = make_uuid5(node_id_, "subscription-" + resource_path +
                                     "-" + std::to_string(subscriptions_.size()));
      sub.resource_path = resource_path;
      sub.source_id     = make_uuid5(sub.id, "ws-source");
      sub.flow_id       = make_uuid5(sub.id, "ws-flow");
      sub.persist       = persist;

      {
        std::lock_guard lk(subscriptions_mutex_);
        subscriptions_[sub.id] = sub;
      }

      std::string body = subscription_json(sub);
      set_nmos_headers(res);
      res.status = 201;
      res.set_content(body, "application/json");
    });

  nmos_get(R"(/x-nmos/query/v1.3/subscriptions/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::string id = req.matches[1];
      std::lock_guard lk(subscriptions_mutex_);
      auto it = subscriptions_.find(id);
      if (it == subscriptions_.end()) { nmos_not_found(res); return; }
      nmos_ok(res, subscription_json(it->second));
    });

  nmos_delete(R"(/x-nmos/query/v1.3/subscriptions/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::string id = req.matches[1];
      std::lock_guard lk(subscriptions_mutex_);
      auto it = subscriptions_.find(id);
      if (it == subscriptions_.end()) { nmos_not_found(res); return; }
      if (it->second.persist) {
        set_nmos_headers(res);
        res.status = 403;
        res.set_content(R"({"code":403,"error":"Persistent subscriptions cannot be deleted"})",
                        "application/json");
        return;
      }
      subscriptions_.erase(it);
      res.status = 204;
      set_nmos_headers(res);
    });

  // ---- Nodes ----
  nmos_get("/x-nmos/query/v1.3/nodes/",
    [this](const NmosReq& req, NmosRes& res) {
      if (has_rql(req)) {
        set_nmos_headers(res);
        res.status = 501;
        res.set_content(R"({"code":501,"error":"Not Implemented","debug":"RQL not supported"})",
                        "application/json");
        return;
      }
      auto id_f    = req.get_param_value("id");
      auto label_f = req.get_param_value("label");
      auto node_js = build_node_json();
      bool match =
          (id_f.empty()    || id_f    == node_id_) &&
          (label_f.empty() || json_field_matches(node_js, "label", label_f));
      query_ok(res, match ? "[" + node_js + "]" : "[]", match ? 1 : 0);
    });
  nmos_get(R"(/x-nmos/query/v1.3/nodes/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      if (req.matches[1] == node_id_)
        nmos_ok(res, build_node_json());
      else
        nmos_not_found(res);
    });

  // ---- Devices ----
  nmos_get("/x-nmos/query/v1.3/devices/",
    [this](const NmosReq& req, NmosRes& res) {
      if (has_rql(req)) {
        set_nmos_headers(res);
        res.status = 501;
        res.set_content(R"({"code":501,"error":"Not Implemented","debug":"RQL not supported"})",
                        "application/json");
        return;
      }
      auto id_f    = req.get_param_value("id");
      auto label_f = req.get_param_value("label");
      std::shared_lock lock(resources_mutex_);
      bool match =
          (id_f.empty()    || id_f    == device_id_) &&
          (label_f.empty() || json_field_matches(device_json_, "label", label_f));
      query_ok(res, match ? "[" + device_json_ + "]" : "[]", match ? 1 : 0);
    });
  nmos_get(R"(/x-nmos/query/v1.3/devices/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::shared_lock lock(resources_mutex_);
      if (req.matches[1] == device_id_)
        nmos_ok(res, device_json_);
      else
        nmos_not_found(res);
    });

  // ---- Sources ----
  nmos_get("/x-nmos/query/v1.3/sources/",
    [this](const NmosReq& req, NmosRes& res) {
      if (has_rql(req)) {
        set_nmos_headers(res);
        res.status = 501;
        res.set_content(R"({"code":501,"error":"Not Implemented","debug":"RQL not supported"})",
                        "application/json");
        return;
      }
      auto id_f    = req.get_param_value("id");
      auto label_f = req.get_param_value("label");
      std::shared_lock lock(resources_mutex_);
      std::ostringstream ss;
      ss << "["; bool first = true; size_t count = 0;
      for (const auto& [id, sr] : senders_) {
        if (!id_f.empty()    && sr.source_id != id_f) continue;
        if (!label_f.empty() && !json_field_matches(sr.source_json, "label", label_f)) continue;
        if (!first) ss << ", ";
        ss << sr.source_json;
        first = false; ++count;
      }
      ss << "]";
      query_ok(res, ss.str(), count);
    });
  nmos_get(R"(/x-nmos/query/v1.3/sources/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, sr] : senders_)
        if (sr.source_id == uuid) { nmos_ok(res, sr.source_json); return; }
      nmos_not_found(res);
    });

  // ---- Flows ----
  nmos_get("/x-nmos/query/v1.3/flows/",
    [this](const NmosReq& req, NmosRes& res) {
      if (has_rql(req)) {
        set_nmos_headers(res);
        res.status = 501;
        res.set_content(R"({"code":501,"error":"Not Implemented","debug":"RQL not supported"})",
                        "application/json");
        return;
      }
      auto id_f    = req.get_param_value("id");
      auto label_f = req.get_param_value("label");
      std::shared_lock lock(resources_mutex_);
      std::ostringstream ss;
      ss << "["; bool first = true; size_t count = 0;
      for (const auto& [id, sr] : senders_) {
        if (!id_f.empty()    && sr.flow_id != id_f) continue;
        if (!label_f.empty() && !json_field_matches(sr.flow_json, "label", label_f)) continue;
        if (!first) ss << ", ";
        ss << sr.flow_json;
        first = false; ++count;
      }
      ss << "]";
      query_ok(res, ss.str(), count);
    });
  nmos_get(R"(/x-nmos/query/v1.3/flows/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, sr] : senders_)
        if (sr.flow_id == uuid) { nmos_ok(res, sr.flow_json); return; }
      nmos_not_found(res);
    });

  // ---- Senders ----
  nmos_get("/x-nmos/query/v1.3/senders/",
    [this](const NmosReq& req, NmosRes& res) {
      if (has_rql(req)) {
        set_nmos_headers(res);
        res.status = 501;
        res.set_content(R"({"code":501,"error":"Not Implemented","debug":"RQL not supported"})",
                        "application/json");
        return;
      }
      auto id_f    = req.get_param_value("id");
      auto label_f = req.get_param_value("label");
      std::shared_lock lock(resources_mutex_);
      std::ostringstream ss;
      ss << "["; bool first = true; size_t count = 0;
      for (const auto& [id, sr] : senders_) {
        if (!id_f.empty()    && sr.sender_id != id_f) continue;
        if (!label_f.empty() && !json_field_matches(sr.sender_json, "label", label_f)) continue;
        if (!first) ss << ", ";
        ss << sr.sender_json;
        first = false; ++count;
      }
      ss << "]";
      query_ok(res, ss.str(), count);
    });
  nmos_get(R"(/x-nmos/query/v1.3/senders/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, sr] : senders_)
        if (sr.sender_id == uuid) { nmos_ok(res, sr.sender_json); return; }
      nmos_not_found(res);
    });

  // ---- Receivers ----
  nmos_get("/x-nmos/query/v1.3/receivers/",
    [this](const NmosReq& req, NmosRes& res) {
      if (has_rql(req)) {
        set_nmos_headers(res);
        res.status = 501;
        res.set_content(R"({"code":501,"error":"Not Implemented","debug":"RQL not supported"})",
                        "application/json");
        return;
      }
      auto id_f    = req.get_param_value("id");
      auto label_f = req.get_param_value("label");
      std::shared_lock lock(resources_mutex_);
      std::ostringstream ss;
      ss << "["; bool first = true; size_t count = 0;
      for (const auto& [id, rr] : receivers_) {
        if (!id_f.empty()    && rr.receiver_id != id_f) continue;
        auto json = build_receiver_json(rr);
        if (!label_f.empty() && !json_field_matches(json, "label", label_f)) continue;
        if (!first) ss << ", ";
        ss << json;
        first = false; ++count;
      }
      ss << "]";
      query_ok(res, ss.str(), count);
    });
  nmos_get(R"(/x-nmos/query/v1.3/receivers/([^/]+))",
    [this](const NmosReq& req, NmosRes& res) {
      std::string uuid = req.matches[1];
      std::shared_lock lock(resources_mutex_);
      for (const auto& [id, rr] : receivers_)
        if (rr.receiver_id == uuid) { nmos_ok(res, build_receiver_json(rr)); return; }
      nmos_not_found(res);
    });
}

void NmosManager::serve_connection(int fd) {
  namespace net       = boost::asio;
  namespace beast     = boost::beast;
  namespace http      = beast::http;
  namespace websocket = beast::websocket;
  using tcp = net::ip::tcp;

  try {
    net::io_context local_ioc;
    tcp::socket raw_sock{local_ioc, tcp::v4(), fd};
    beast::tcp_stream stream{std::move(raw_sock)};
    beast::flat_buffer buf;

    http::request<http::string_body> req;
    http::read(stream, buf, req);

    if (websocket::is_upgrade(req)) {
      // --- WebSocket upgrade path ---
      std::string target{req.target()};

      const std::string ncp_prefix{"/x-nmos/ncp/v1.0"};
      if (config_->get_is12_enabled() && target.rfind(ncp_prefix, 0) == 0) {
        websocket::stream<beast::tcp_stream> ncp_ws{std::move(stream)};
        ncp_ws.set_option(websocket::stream_base::timeout::suggested(beast::role_type::server));
        ncp_ws.accept(req);
        serve_is12_connection(ncp_ws);
        {
          boost::system::error_code ec;
          ncp_ws.close(websocket::close_code::normal, ec);
        }
        return;
      }

      const std::string ws_prefix{"/x-nmos/query/v1.3/subscriptions/"};
      std::string sub_id;
      if (target.rfind(ws_prefix, 0) == 0) {
        sub_id = target.substr(ws_prefix.size());
        while (!sub_id.empty() && sub_id.back() == '/') sub_id.pop_back();
      }

      std::string resource_path, grain_source_id, grain_flow_id;
      bool found = false;
      {
        std::lock_guard lk(subscriptions_mutex_);
        auto it = subscriptions_.find(sub_id);
        if (it != subscriptions_.end()) {
          resource_path   = it->second.resource_path;
          grain_source_id = it->second.source_id;
          grain_flow_id   = it->second.flow_id;
          found = true;
        }
      }

      if (!found) {
        http::response<http::string_body> resp{http::status::not_found, req.version()};
        resp.set(http::field::content_type, "text/plain");
        resp.body() = "Subscription not found";
        resp.prepare_payload();
        http::write(stream, resp);
        return;
      }

      websocket::stream<beast::tcp_stream> ws{std::move(stream)};
      ws.set_option(websocket::stream_base::timeout::suggested(beast::role_type::server));
      ws.accept(req);

      BOOST_LOG_TRIVIAL(debug)
          << "NmosManager:: WS client connected for " << resource_path;

      std::string grain = build_initial_grain(resource_path, grain_source_id, grain_flow_id);
      ws.text(true);
      ws.write(net::buffer(grain));

      while (running_) {
        beast::flat_buffer rbuf;
        boost::system::error_code ec;
        ws.read(rbuf, ec);
        if (ec == websocket::error::closed || ec) break;
      }

      {
        boost::system::error_code ec;
        ws.close(websocket::close_code::normal, ec);
      }
      BOOST_LOG_TRIVIAL(debug)
          << "NmosManager:: WS client disconnected for " << resource_path;

      {
        std::lock_guard lk(subscriptions_mutex_);
        auto it = subscriptions_.find(sub_id);
        if (it != subscriptions_.end() && !it->second.persist)
          subscriptions_.erase(it);
      }
    } else {
      // --- Plain HTTP path: dispatch through nmos_routes_ ---
      std::string method{req.method_string()};
      std::string target{req.target()};
      std::string path = target, qs;
      auto qpos = target.find('?');
      if (qpos != std::string::npos) {
        path = target.substr(0, qpos);
        qs   = target.substr(qpos + 1);
      }

      NmosReq nreq;
      nreq.body = req.body();
      nreq.qs_  = qs;
      NmosRes nres;

      // Normalize: try path as-is, then with trailing slash (clients often omit it)
      std::string path_slash = (path.empty() || path.back() == '/') ? "" : path + '/';
      bool matched = false;
      for (const auto& route : nmos_routes_) {
        if (route.method != method) continue;
        std::smatch m;
        if (std::regex_match(path, m, route.pattern) ||
            (!path_slash.empty() && std::regex_match(path_slash, m, route.pattern))) {
          nreq.matches = m;
          route.handler(nreq, nres);
          matched = true;
          break;
        }
      }
      if (!matched) {
        nres.status = 404;
        nres.body_  = "{\"code\":404,\"error\":\"Not Found\"}";
      }

      http::response<http::string_body> resp{
          static_cast<http::status>(nres.status), req.version()};
      resp.set(http::field::content_type, nres.ct_);
      for (const auto& [k, v] : nres.hdrs_) resp.set(k, v);
      resp.body() = nres.body_;
      resp.prepare_payload();
      http::write(stream, resp);
    }
  } catch (const boost::beast::system_error& e) {
    if (e.code() != websocket::error::closed)
      BOOST_LOG_TRIVIAL(debug) << "NmosManager:: connection: " << e.what();
  } catch (const std::exception& e) {
    BOOST_LOG_TRIVIAL(debug) << "NmosManager:: connection error: " << e.what();
  }
}

bool NmosManager::server_worker() {
  namespace net = boost::asio;
  using tcp = net::ip::tcp;

  try {
    net::io_context ioc;
    auto port = static_cast<uint16_t>(config_->get_nmos_node_port());
    tcp::acceptor acceptor{ioc, {net::ip::address_v4::any(), port}};

    struct timeval tv{1, 0};
    setsockopt(acceptor.native_handle(), SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));

    BOOST_LOG_TRIVIAL(info)
        << "NmosManager:: HTTP+WebSocket server on port " << port;

    while (running_) {
      tcp::socket sock{ioc};
      boost::system::error_code ec;
      acceptor.accept(sock, ec);
      if (!running_) break;
      if (ec.value() == EAGAIN || ec.value() == EWOULDBLOCK) continue;
      if (ec) {
        BOOST_LOG_TRIVIAL(error)
            << "NmosManager:: accept error: " << ec.message();
        break;
      }
      int fd = sock.release();
      std::thread(&NmosManager::serve_connection, this, fd).detach();
    }
  } catch (const std::exception& e) {
    BOOST_LOG_TRIVIAL(error) << "NmosManager:: server exception: " << e.what();
  }
  BOOST_LOG_TRIVIAL(info) << "NmosManager:: HTTP+WebSocket server stopped";
  return true;
}

// ---------------------------------------------------------------------------
// Registration client helpers
// ---------------------------------------------------------------------------

bool NmosManager::register_resource(const std::string& type,
                                     const std::string& data_json) {
  httplib::Client cli(effective_registry_address(),
                      effective_registry_port());
  cli.set_connection_timeout(5, 0);
  cli.set_read_timeout(10, 0);

  std::string body = "{\"type\": \"" + type + "\", \"data\": " + data_json + "}";
  auto res = cli.Post("/x-nmos/registration/v1.3/resource", body, "application/json");
  if (!res) {
    BOOST_LOG_TRIVIAL(error) << "NmosManager:: register " << type
                             << " failed (no response)";
    return false;
  }
  if (res->status != 200 && res->status != 201) {
    BOOST_LOG_TRIVIAL(error) << "NmosManager:: register " << type
                             << " returned HTTP " << res->status;
    return false;
  }
  BOOST_LOG_TRIVIAL(debug) << "NmosManager:: registered " << type;
  return true;
}

bool NmosManager::unregister_resource(const std::string& type,
                                       const std::string& id) {
  httplib::Client cli(effective_registry_address(),
                      effective_registry_port());
  cli.set_connection_timeout(5, 0);
  cli.set_read_timeout(10, 0);

  std::string path = "/x-nmos/registration/v1.3/resource/" + type + "s/" + id;
  auto res = cli.Delete(path.c_str());
  if (!res) {
    BOOST_LOG_TRIVIAL(warning) << "NmosManager:: unregister " << type << " " << id
                               << " failed (no response)";
    return false;
  }
  if (res->status != 204) {
    BOOST_LOG_TRIVIAL(warning) << "NmosManager:: unregister " << type
                               << " returned HTTP " << res->status;
    return false;
  }
  return true;
}

bool NmosManager::heartbeat() {
  httplib::Client cli(effective_registry_address(),
                      effective_registry_port());
  cli.set_connection_timeout(5, 0);
  cli.set_read_timeout(10, 0);

  std::string path = "/x-nmos/registration/v1.3/health/nodes/" + node_id_;
  auto res = cli.Post(path.c_str(), "", "application/json");
  if (!res) {
    BOOST_LOG_TRIVIAL(warning) << "NmosManager:: heartbeat failed (no response)";
    return false;
  }
  if (res->status == 404) {
    BOOST_LOG_TRIVIAL(warning) << "NmosManager:: node expired from registry, re-registering";
    return full_registration();
  }
  if (res->status != 200) {
    BOOST_LOG_TRIVIAL(warning) << "NmosManager:: heartbeat returned HTTP "
                               << res->status;
    return false;
  }
  return true;
}

// ---------------------------------------------------------------------------
// Resource registration
// ---------------------------------------------------------------------------

bool NmosManager::register_source_local(uint8_t id) {
  StreamSource src;
  if (auto ec = session_manager_->get_source(id, src); ec) {
    BOOST_LOG_TRIVIAL(error) << "NmosManager:: get_source(" << +id
                             << ") failed: " << ec.message();
    return false;
  }
  std::string source_id = make_resource_uuid("source", id);
  std::string flow_id   = make_resource_uuid("flow",   id);
  std::string sender_id = make_resource_uuid("sender", id);
  auto tp = build_sender_tp(src);
  std::unique_lock lock(resources_mutex_);
  SenderResources& sr   = senders_[id];
  sr.source_id          = source_id;
  sr.flow_id            = flow_id;
  sr.sender_id          = sender_id;
  sr.source_json        = build_source_json(src, source_id);
  sr.flow_json          = build_flow_json(src, source_id, flow_id);
  sr.sender_json        = build_sender_json(src, id, flow_id, sender_id, "");
  sr.staged_master_enable = src.enabled;
  sr.staged_tp            = tp;
  sr.active_master_enable = src.enabled;
  sr.active_tp            = tp;
  rebuild_device_json_locked();
  return true;
}

bool NmosManager::register_source(uint8_t id) {
  if (!register_source_local(id)) return false;
  std::string src_json, fl_json, snd_json, dev_json;
  {
    std::shared_lock lock(resources_mutex_);
    auto it = senders_.find(id);
    if (it == senders_.end()) return false;
    src_json = it->second.source_json;
    fl_json  = it->second.flow_json;
    snd_json = it->second.sender_json;
    dev_json = device_json_;
  }
  // Best-effort registry push — failures are logged but not fatal.
  register_resource("source", src_json);
  register_resource("flow",   fl_json);
  register_resource("sender", snd_json);
  register_resource("device", dev_json);
  return true;
}

bool NmosManager::unregister_source(uint8_t id) {
  SenderResources sr;
  std::string dev_json;
  {
    std::unique_lock lock(resources_mutex_);
    auto it = senders_.find(id);
    if (it == senders_.end()) return true;
    sr = it->second;
    senders_.erase(it);
    rebuild_device_json_locked();
    dev_json = device_json_;
  }
  register_resource("device", dev_json);
  unregister_resource("sender", sr.sender_id);
  unregister_resource("flow",   sr.flow_id);
  unregister_resource("source", sr.source_id);
  return true;
}

bool NmosManager::register_sink_local(uint8_t id) {
  StreamSink sink;
  if (auto ec = session_manager_->get_sink(id, sink); ec) {
    BOOST_LOG_TRIVIAL(error) << "NmosManager:: get_sink(" << +id
                             << ") failed: " << ec.message();
    return false;
  }
  std::string receiver_id = make_resource_uuid("receiver", id);
  std::vector<ReceiverTp> tps;
  if (sink.use_sdp && !sink.sdp.empty()) {
    tps = build_receiver_tp_from_sdp(sink.sdp);
  } else {
    ReceiverTp tp0;
    tp0.interface_ip = config_->get_ip_addr_str();
    tps.push_back(tp0);
    if (is_dual_leg()) {
      ReceiverTp tp1;
      tp1.interface_ip = sec_interface_ip_str_;
      tps.push_back(tp1);
    }
  }
  bool connected = sink.use_sdp && !sink.sdp.empty();
  std::unique_lock lock(resources_mutex_);
  ReceiverResources& rr   = receivers_[id];
  rr.receiver_id          = receiver_id;
  rr.sink                 = sink;
  rr.staged_master_enable = connected;
  rr.staged_tp            = tps;
  rr.active_master_enable = connected;
  rr.active_tp            = tps;
  // Restore IS-05 active sender preserved through a remove+add cycle
  auto pres = preserved_active_sender_ids_.find(id);
  if (pres != preserved_active_sender_ids_.end()) {
    rr.active_sender_id  = pres->second;
    preserved_active_sender_ids_.erase(pres);
  }
  rebuild_device_json_locked();
  return true;
}

bool NmosManager::register_sink(uint8_t id) {
  if (!register_sink_local(id)) return false;
  std::string rcv_json, dev_json;
  {
    std::shared_lock lock(resources_mutex_);
    auto it = receivers_.find(id);
    if (it == receivers_.end()) return false;
    rcv_json = build_receiver_json(it->second);
    dev_json = device_json_;
  }
  // Best-effort registry push.
  register_resource("receiver", rcv_json);
  register_resource("device", dev_json);
  return true;
}

bool NmosManager::unregister_sink(uint8_t id) {
  ReceiverResources rr;
  std::string dev_json;
  {
    std::unique_lock lock(resources_mutex_);
    auto it = receivers_.find(id);
    if (it == receivers_.end()) return true;
    rr = it->second;
    receivers_.erase(it);
    rebuild_device_json_locked();
    dev_json = device_json_;
  }
  register_resource("device", dev_json);
  unregister_resource("receiver", rr.receiver_id);
  return true;
}

bool NmosManager::full_registration() {
  // Re-sync local state in case any resources were added between init()'s
  // pre-population and now (e.g., loaded from status file asynchronously).
  for (const auto& src : session_manager_->get_sources())
    register_source_local(src.id);
  for (const auto& sink : session_manager_->get_sinks())
    register_sink_local(sink.id);

  // Refresh node JSON so the registry gets the current PTP clock state.
  node_json_ = build_node_json();

  // Collect all JSON strings under shared lock, then push to registry outside
  // the lock so PATCH requests are not blocked during slow network I/O.
  std::vector<std::pair<std::string, std::string>> to_push;
  to_push.emplace_back("node", node_json_);
  {
    std::shared_lock lock(resources_mutex_);
    to_push.emplace_back("device", device_json_);
    for (const auto& [id, sr] : senders_) {
      to_push.emplace_back("source", sr.source_json);
      to_push.emplace_back("flow",   sr.flow_json);
      to_push.emplace_back("sender", sr.sender_json);
    }
    for (const auto& [id, rr] : receivers_)
      to_push.emplace_back("receiver", build_receiver_json(rr));
  }

  BOOST_LOG_TRIVIAL(info) << "NmosManager:: registering with registry at "
                          << effective_registry_address() << ":"
                          << effective_registry_port();
  for (const auto& [type, json] : to_push)
    register_resource(type, json);

  return true;
}

// ---------------------------------------------------------------------------
// Observer callbacks — push events to the queue
// ---------------------------------------------------------------------------

bool NmosManager::on_source_added(uint8_t id, const std::string& /*name*/,
                                   const std::string& /*sdp*/) {
  if (!running_) return true;
  {
    std::unique_lock lock(events_mutex_);
    pending_events_.push({EventType::SourceAdded, id});
  }
  events_cv_.notify_one();
  return true;
}

bool NmosManager::on_source_removed(uint8_t id, const std::string& /*name*/,
                                     const std::string& /*sdp*/) {
  if (!running_) return true;
  {
    std::unique_lock lock(events_mutex_);
    pending_events_.push({EventType::SourceRemoved, id});
  }
  events_cv_.notify_one();
  return true;
}

bool NmosManager::on_sink_added(uint8_t id, const std::string& /*name*/) {
  if (!running_) return true;
  {
    std::unique_lock lock(events_mutex_);
    pending_events_.push({EventType::SinkAdded, id});
  }
  events_cv_.notify_one();
  return true;
}

bool NmosManager::on_sink_removed(uint8_t id, const std::string& /*name*/) {
  if (!running_) return true;
  {
    std::unique_lock lock(events_mutex_);
    pending_events_.push({EventType::SinkRemoved, id});
  }
  events_cv_.notify_one();
  return true;
}

// ---------------------------------------------------------------------------
// Registration worker — processes events and heartbeats
// ---------------------------------------------------------------------------

bool NmosManager::registration_worker() {
  // Give the Node API server a moment to start listening
  std::this_thread::sleep_for(std::chrono::milliseconds(500));

  full_registration();

  using clock = std::chrono::steady_clock;
  auto next_hb = clock::now() + std::chrono::seconds(5);

  while (running_) {
    // Drain pending events (wait up to 1 s for the next one)
    {
      std::unique_lock lock(events_mutex_);
      events_cv_.wait_for(lock, std::chrono::seconds(1),
                          [this] { return !pending_events_.empty() || !running_; });

      while (!pending_events_.empty()) {
        Event ev = pending_events_.front();
        pending_events_.pop();
        lock.unlock();

        switch (ev.type) {
          case EventType::SourceAdded:    register_source(ev.id);   break;
          case EventType::SourceRemoved:  unregister_source(ev.id); break;
          case EventType::SinkAdded:      register_sink(ev.id);     break;
          case EventType::SinkRemoved:    unregister_sink(ev.id);   break;
          case EventType::RegistryUpdated:
            BOOST_LOG_TRIVIAL(info)
                << "NmosManager:: DNS-SD registry available, re-registering";
            full_registration();
            break;
          case EventType::RegistryLost:
            BOOST_LOG_TRIVIAL(info)
                << "NmosManager:: DNS-SD registry lost";
            break;
        }

        lock.lock();
      }
    }

    // Process any scheduled IS-05 activations that have come due
    process_scheduled_activations();

    // Same for IS-08 (see nmos_is08.cpp — a separate pending-activations map,
    // same "check every wake of this loop" approach as above rather than a
    // dedicated thread)
    if (config_->get_is08_enabled()) is08_process_scheduled_activations();

    // Heartbeat when due
    if (running_ && clock::now() >= next_hb) {
      heartbeat();
      // A heartbeat carries no body, so without this the registry's copy of
      // the Node resource (clock lock state, GMID, active leg...) would stay
      // frozen at whatever it looked like the moment full_registration()
      // last ran, even though GET /x-nmos/node/v1.3/self (queried directly
      // against this daemon) keeps reporting the real, current state.
      node_json_ = build_node_json();
      register_resource("node", node_json_);
      next_hb = clock::now() + std::chrono::seconds(5);
    }
  }

  // Unregister node on graceful shutdown (registry will GC the rest)
  unregister_resource("node", node_id_);
  BOOST_LOG_TRIVIAL(info) << "NmosManager:: registration worker stopped";
  return true;
}
