//
//  nmos_manager.hpp
//
//  IS-04 Node API server, IS-05 Connection Management, and Registration client.
//  Sources → NMOS Senders (+ Source + Flow); Sinks → NMOS Receivers.
//

#ifndef _NMOS_MANAGER_HPP_
#define _NMOS_MANAGER_HPP_

#ifdef _USE_AVAHI_
#include <avahi-client/client.h>
#include <avahi-client/lookup.h>
#include <avahi-common/error.h>
#include <avahi-common/malloc.h>
#include <avahi-common/thread-watch.h>
#endif

#include <atomic>
#include <condition_variable>
#include <deque>
#include <functional>
#include <future>
#include <map>
#include <mutex>
#include <queue>
#include <regex>
#include <set>
#include <shared_mutex>
#include <string>
#include <utility>
#include <vector>

#include <boost/beast/core.hpp>
#include <boost/beast/websocket.hpp>

#include "config.hpp"
#include "ptp_clock_shm.hpp"
#include "session_manager.hpp"

class NmosManager {
 public:
  static std::shared_ptr<NmosManager> create(
      std::shared_ptr<SessionManager> session_manager,
      std::shared_ptr<Config> config);
  NmosManager() = delete;
  NmosManager(std::shared_ptr<SessionManager> sm, std::shared_ptr<Config> cfg)
      : session_manager_(std::move(sm)), config_(std::move(cfg)) {}
  NmosManager(const NmosManager&) = delete;
  NmosManager& operator=(const NmosManager&) = delete;
  virtual ~NmosManager() = default;

  bool init();
  bool terminate();

 // ---- Minimal HTTP request/response types used by route handlers ----
 //      Public so file-scope static helpers in nmos_manager.cpp can name them.
 public:
  struct NmosReq {
    std::string body;
    std::smatch matches;
    std::string qs_;  // raw query string (after '?')

    std::string get_param_value(const std::string& key) const {
      if (qs_.empty()) return "";
      const std::string kv = key + "=";
      auto pos = qs_.find(kv);
      while (pos != std::string::npos) {
        if (pos == 0 || qs_[pos - 1] == '&') {
          pos += kv.size();
          auto end = qs_.find('&', pos);
          return qs_.substr(pos,
              end == std::string::npos ? std::string::npos : end - pos);
        }
        pos = qs_.find(kv, pos + 1);
      }
      return "";
    }
  };

  struct NmosRes {
    int         status{200};
    std::string body_;
    std::string ct_{"application/json"};
    std::vector<std::pair<std::string, std::string>> hdrs_;

    void set_content(const std::string& b, const std::string& ct) {
      body_ = b;
      ct_   = ct;
    }
    void set_header(const std::string& k, const std::string& v) {
      hdrs_.emplace_back(k, v);
    }
  };

  using NmosHandler = std::function<void(const NmosReq&, NmosRes&)>;

  struct NmosRoute {
    std::string method;
    std::regex  pattern;
    NmosHandler handler;
  };

 private:
  // IS-05 activation record
  struct Is05Activation {
    std::string mode;            // "" = null
    std::string requested_time; // "" = null
    std::string activation_time;// "" = null
    int64_t     deadline_ns{-1};// steady_clock ns; -1 = not scheduled
  };

  // IS-05 RTP transport parameters for a sender leg
  struct SenderTp {
    std::string source_ip;
    std::string destination_ip;
    uint16_t    source_port{5004};
    uint16_t    destination_port{5004};
    bool        rtp_enabled{false};
  };

  // IS-05 RTP transport parameters for a receiver leg
  struct ReceiverTp {
    std::string interface_ip;
    std::string multicast_ip;        // "" = null
    uint16_t    destination_port{5004};
    std::string source_ip{"auto"};
    bool        rtp_enabled{false};
  };

  struct SenderResources {
    // IS-04
    std::string source_id, flow_id, sender_id;
    std::string source_json, flow_json, sender_json;
    // IS-05 staged
    bool        staged_master_enable{true};
    std::string staged_receiver_id; // "" = null
    Is05Activation staged_act;
    std::vector<SenderTp> staged_tp;
    // IS-05 active (copy of staged after activation)
    bool        active_master_enable{true};
    std::string active_receiver_id;
    Is05Activation active_act;
    std::vector<SenderTp> active_tp;
  };

  struct ReceiverResources {
    // IS-04
    std::string receiver_id;
    StreamSink  sink;
    // IS-05 staged
    bool        staged_master_enable{false};
    std::string staged_sender_id;   // "" = null
    Is05Activation staged_act;
    std::vector<ReceiverTp> staged_tp;
    // IS-05 active
    bool        active_master_enable{false};
    std::string active_sender_id;
    Is05Activation active_act;
    std::vector<ReceiverTp> active_tp;
  };

  // Scheduled activation awaiting its deadline
  struct PendingActivation {
    bool    is_sender;
    uint8_t daemon_id;
    int64_t deadline_ns;
  };

  enum class EventType { SourceAdded, SourceRemoved, SinkAdded, SinkRemoved,
                         RegistryUpdated, RegistryLost };
  struct Event { EventType type; uint8_t id; };

  void nmos_add(const std::string& m, const std::string& p, NmosHandler h) {
    nmos_routes_.push_back({m, std::regex(p), std::move(h)});
  }
  void nmos_get    (const std::string& p, NmosHandler h) { nmos_add("GET",     p, h); }
  void nmos_post   (const std::string& p, NmosHandler h) { nmos_add("POST",    p, h); }
  void nmos_put    (const std::string& p, NmosHandler h) { nmos_add("PUT",     p, h); }
  void nmos_patch  (const std::string& p, NmosHandler h) { nmos_add("PATCH",   p, h); }
  void nmos_delete (const std::string& p, NmosHandler h) { nmos_add("DELETE",  p, h); }
  void nmos_options(const std::string& p, NmosHandler h) { nmos_add("OPTIONS", p, h); }

  void serve_connection(int fd);  // owns fd; handles HTTP and WebSocket

  // ---- IS-04 ----
  void setup_node_api();
  void setup_query_api();
  void rebuild_device_json_locked();

  // IS-04 Query API WebSocket subscriptions
  struct Subscription {
    std::string id;
    std::string resource_path;
    std::string source_id;  // deterministic UUID for grain source
    std::string flow_id;    // deterministic UUID for grain flow
    bool persist{false};
  };

  std::string build_initial_grain(const std::string& resource_path,
                                   const std::string& grain_source_id,
                                   const std::string& grain_flow_id) const;
  std::string subscription_json(const Subscription& sub) const;

  std::string make_resource_uuid(const std::string& type, uint8_t id) const;
  std::string build_node_json() const;
  std::string build_source_json(const StreamSource& src,
                                const std::string& source_id) const;
  std::string build_flow_json(const StreamSource& src,
                              const std::string& source_id,
                              const std::string& flow_id) const;
  std::string build_sender_json(const StreamSource& src,
                                uint8_t daemon_id,
                                const std::string& flow_id,
                                const std::string& sender_id,
                                const std::string& active_receiver_id) const;
  std::string build_receiver_json(const StreamSink& sink,
                                  const std::string& receiver_id,
                                  const std::string& active_sender_id) const;
  std::string build_receiver_json(const ReceiverResources& rr) const;

  // ---- IS-05 ----
  void setup_connection_api();

  std::vector<SenderTp>   build_sender_tp(const StreamSource& src) const;
  std::vector<ReceiverTp> build_receiver_tp_from_sdp(const std::string& sdp) const;
  bool is_dual_leg() const { return !config_->get_interface_name(1).empty(); }

  std::string tp_sender_json(const SenderTp& tp) const;
  std::string tp_receiver_json(const ReceiverTp& tp) const;
  std::string activation_json(const Is05Activation& act) const;
  std::string staged_sender_json(const SenderResources& sr) const;
  std::string active_sender_json(const SenderResources& sr) const;
  std::string staged_receiver_json(const ReceiverResources& rr) const;
  std::string active_receiver_json(const ReceiverResources& rr) const;

  bool patch_sender_staged(uint8_t daemon_id,
                           const std::string& body,
                           std::string& error_out,
                           std::string& staged_json_out);
  bool patch_receiver_staged(uint8_t daemon_id,
                             const std::string& body,
                             std::string& error_out,
                             std::string& staged_json_out);

  void apply_sender_activation(uint8_t daemon_id);
  void apply_receiver_activation(uint8_t daemon_id);
  void fetch_remote_sender_sdp(const std::string& sender_uuid, std::string& sdp);
  void process_scheduled_activations();

  // ---- ptp-clock-manager integration ----
  // ptp-clock-manager is an optional standalone process (see
  // ../ptp-clock-manager/) that disciplines the local system clock from the
  // RAVENNA PTP grandmaster and reports its own lock state via shared
  // memory. When it's running, its lock state is a better signal than the
  // driver's raw PTP message reception (session_manager_->get_ptp_status):
  // it reflects whether the local clock has actually converged, not just
  // whether PTP messages are being seen. Used by both build_node_json
  // (IS-04 self.clocks) and the IS-12 monitor sync-status properties.
  struct PtpSyncInfo {
    bool available{false};  // ptp-clock-manager running and shm snapshot fresh
    bool locked{false};
    bool locking{false};
    std::string gmid_dash;  // "xx-xx-xx-xx-xx-xx-xx-xx", set only if available
    int64_t offset_ns{0};
    int64_t freq_ppb{0};
  };
  PtpSyncInfo get_ptp_clock_manager_sync() const;
  void ncp_sync_status(int& status, std::string& message) const;

  // ---- IS-12 (NMOS Control Protocol) ----
  // Property value already encoded as a JSON literal ready to splice into a
  // response/notification body — keeps the daemon's "hand-rolled ostringstream
  // JSON" convention instead of introducing a variant/JSON-value type.
  struct NcPropEntry {
    int level;
    int index;
    std::string json_value;
  };

  // One IS-12 WebSocket connection: the connection's own thread blocks in
  // ws.read() (see serve_connection) while a companion writer thread drains
  // this queue — Boost.Beast allows one concurrent reader + one concurrent
  // writer on the same stream, so this is the only way to push async
  // Notifications without blocking (or being blocked by) that read loop.
  struct Is12Session {
    std::mutex              mtx;
    std::condition_variable cv;
    std::deque<std::string> outbox;
    std::set<long>          subscribed;
    bool                    closing{false};
  };

  void serve_is12_connection(
      boost::beast::websocket::stream<boost::beast::tcp_stream>& ws);
  void handle_is12_message(const std::string& msg,
                           const std::shared_ptr<Is12Session>& session);
  bool is12_notify_worker();

  std::vector<NcPropEntry> ncp_receiver_monitor_props(uint8_t sink_id) const;
  std::vector<NcPropEntry> ncp_sender_monitor_props(uint8_t source_id) const;
  std::string ncp_member_descriptors_json() const;
  bool ncp_class_descriptor_json(const std::vector<int>& class_id,
                                 std::string& out) const;

  std::mutex                              is12_sessions_mutex_;
  std::vector<std::shared_ptr<Is12Session>> is12_sessions_;
  std::future<bool>                       is12_notify_res_;

  // ---- IS-08 (Audio Channel Mapping) ----
  //
  // Each Sink/Source has exactly two Channel Mapping resources, matching the
  // real ALSA-mediated audio path in this daemon rather than a network-to-
  // network shortcut:
  //   Sink:   Input  = its stream (RX) side   — channels = sink.map.size()
  //           Output = its ALSA side          — channels = get_number_of_inputs()
  //             (a Sink writes RX'd audio into ALSA *capture* channels —
  //             "inputs" in the driver's own vocabulary, see driver_manager.hpp)
  //   Source: Input  = its ALSA side          — channels = get_number_of_outputs()
  //             (a Source reads TX audio from ALSA *playback* channels)
  //           Output = its stream (TX) side   — channels = source.map.size()
  // A crosspoint activation directly edits that Sink's/Source's own `map[]`
  // (map[stream_channel] = alsa_channel) — there is no cross-Sink-to-Source
  // resource at all. Repatching a Sink's incoming audio out through some
  // Source (a "network repeater") is still possible, just indirect: activate
  // the Sink's Output onto some ALSA channel X, then separately activate that
  // Source's Input from the same ALSA channel X — exactly mirroring how the
  // physical/ALSA path actually works underneath.
  void setup_is08_api();

  enum class Is08Kind { SinkStream, SourceAlsa, SinkAlsa, SourceStream };
  struct Is08Ref {
    Is08Kind kind;
    uint8_t id;  // daemon sink id (SinkStream/SinkAlsa) or source id (SourceAlsa/SourceStream)
  };

  std::string is08_resource_id(Is08Kind kind, uint8_t id) const {
    const char* ns = kind == Is08Kind::SinkStream    ? "cm_sink_stream"
                     : kind == Is08Kind::SinkAlsa     ? "cm_sink_alsa"
                     : kind == Is08Kind::SourceAlsa    ? "cm_source_alsa"
                                                        : "cm_source_stream";
    return make_resource_uuid(ns, id);
  }
  bool find_cm_input(const std::string& uuid, Is08Ref& ref) const;
  bool find_cm_output(const std::string& uuid, Is08Ref& ref) const;

  std::string is08_channels_json(size_t channel_count) const;
  std::string is08_map_active_json() const;

  // ---- Registration ----
  bool register_resource(const std::string& type, const std::string& data_json);
  bool unregister_resource(const std::string& type, const std::string& id);
  bool heartbeat();

  bool full_registration();
  bool register_source_local(uint8_t id);
  bool register_sink_local(uint8_t id);
  bool register_source(uint8_t id);
  bool unregister_source(uint8_t id);
  bool register_sink(uint8_t id);
  bool unregister_sink(uint8_t id);

  bool on_source_added(uint8_t id, const std::string& name, const std::string& sdp);
  bool on_source_removed(uint8_t id, const std::string& name, const std::string& sdp);
  bool on_sink_added(uint8_t id, const std::string& name);
  bool on_sink_removed(uint8_t id, const std::string& name);

  bool registration_worker();
  bool server_worker();

  // ---- Members ----
  std::shared_ptr<SessionManager> session_manager_;
  std::shared_ptr<Config>         config_;

  std::string node_id_;
  std::string device_id_;
  std::string node_json_;

  mutable std::shared_mutex            resources_mutex_;
  std::map<uint8_t, SenderResources>   senders_;
  std::map<uint8_t, ReceiverResources> receivers_;
  std::string device_json_;

  mutable std::mutex             pending_act_mutex_;
  std::vector<PendingActivation> pending_activations_;

  std::map<uint8_t, std::string> preserved_active_sender_ids_;

  std::map<std::string, Subscription> subscriptions_;
  mutable std::mutex                  subscriptions_mutex_;

  std::vector<NmosRoute> nmos_routes_;

  std::atomic_bool  running_{false};
  std::future<bool> reg_res_;
  std::future<bool> svr_res_;

  std::mutex              events_mutex_;
  std::condition_variable events_cv_;
  std::queue<Event>       pending_events_;

  // ---- DNS-SD registry discovery ----
  std::string effective_registry_address() const;
  uint16_t    effective_registry_port() const;

#ifdef _USE_AVAHI_
  void start_registry_discovery();
  void stop_registry_discovery();
  static void registry_client_callback(AvahiClient*, AvahiClientState, void*);
  static void registry_browse_callback(AvahiServiceBrowser*, AvahiIfIndex,
      AvahiProtocol, AvahiBrowserEvent, const char*, const char*, const char*,
      AvahiLookupResultFlags, void*);
  static void registry_resolve_callback(AvahiServiceResolver*, AvahiIfIndex,
      AvahiProtocol, AvahiResolverEvent, const char*, const char*, const char*,
      const char*, const AvahiAddress*, uint16_t, AvahiStringList*,
      AvahiLookupResultFlags, void*);

  std::unique_ptr<AvahiThreadedPoll, decltype(&avahi_threaded_poll_free)>
      registry_poll_{nullptr, &avahi_threaded_poll_free};
  std::unique_ptr<AvahiClient, decltype(&avahi_client_free)>
      registry_avahi_client_{nullptr, &avahi_client_free};
  std::unique_ptr<AvahiServiceBrowser, decltype(&avahi_service_browser_free)>
      registry_browser_{nullptr, &avahi_service_browser_free};
#endif

  mutable std::mutex  registry_disc_mutex_;
  std::string         discovered_registry_address_;
  uint16_t            discovered_registry_port_{0};
  std::string         sec_interface_ip_str_;
};

#endif
