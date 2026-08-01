#include "pipewire_driver.hpp"
#include <cstdio>
#include <cstring>

#ifdef WITH_PIPEWIRE
#include <pipewire/pipewire.h>
#include <pipewire/loop.h>
#include <spa/utils/dict.h>
#endif

PipeWireDriver::PipeWireDriver() = default;
PipeWireDriver::PipeWireDriver(Config cfg) : cfg_(cfg) {}

PipeWireDriver::~PipeWireDriver() {
    stop();
}

void PipeWireDriver::note_tai_driver_node(const std::string& node_name) {
    if (tai_driver_found_.exchange(true))
        return;  /* already reported */
    std::printf("PipeWireDriver: graph driver '%s' is TAI-clocked — "
                "PipeWire is consuming the disciplined CLOCK_TAI\n",
                node_name.c_str());
}

#ifdef WITH_PIPEWIRE

namespace {

struct RegistryCtx {
    PipeWireDriver* self;
    const char*     name_filter;  /* PipeWireDriverConfig::driver_node_name */
    struct spa_hook listener;
};

void on_registry_global(void* data, uint32_t /*id*/, uint32_t /*permissions*/,
                        const char* type, uint32_t /*version*/,
                        const struct spa_dict* props) {
    if (!props || std::strcmp(type, PW_TYPE_INTERFACE_Node) != 0)
        return;

    const char* clock_id = spa_dict_lookup(props, "clock.id");
    if (!clock_id || std::strcmp(clock_id, "tai") != 0)
        return;

    const char* node_name = spa_dict_lookup(props, "node.name");
    auto* ctx = static_cast<RegistryCtx*>(data);
    if (ctx->name_filter && (!node_name || std::strcmp(node_name, ctx->name_filter) != 0))
        return;

    ctx->self->note_tai_driver_node(node_name ? node_name : "(unnamed)");
}

} // namespace

#endif /* WITH_PIPEWIRE */

bool PipeWireDriver::start() {
#ifndef WITH_PIPEWIRE
    std::fputs("PipeWireDriver: not compiled in (rebuild with -DWITH_PIPEWIRE=ON)\n", stderr);
    return false;
#else
    pw_init(nullptr, nullptr);

    pw_loop_ = pw_main_loop_new(nullptr);
    if (!pw_loop_) return false;

    pw_context_ = pw_context_new(
        pw_main_loop_get_loop(static_cast<struct pw_main_loop*>(pw_loop_)),
        nullptr, 0);
    if (!pw_context_) return false;

    pw_core_ = pw_context_connect(
        static_cast<struct pw_context*>(pw_context_), nullptr, 0);
    if (!pw_core_) return false;

    auto* registry = pw_core_get_registry(
        static_cast<struct pw_core*>(pw_core_), PW_VERSION_REGISTRY, 0);
    if (!registry) {
        std::fputs("PipeWireDriver: pw_core_get_registry failed\n", stderr);
        return false;
    }
    pw_registry_ = registry;

    auto* ctx = new RegistryCtx{this, cfg_.driver_node_name, {}};
    registry_ctx_ = ctx;

    struct pw_registry_events events{};
    events.version = PW_VERSION_REGISTRY_EVENTS;
    events.global  = on_registry_global;
    pw_registry_add_listener(registry, &ctx->listener, &events, ctx);

    /* Registry events and the connect handshake only progress while the
     * loop is being pumped, so run it on a dedicated thread. */
    loop_thread_ = std::thread([this] {
        pw_main_loop_run(static_cast<struct pw_main_loop*>(pw_loop_));
    });

    std::puts("PipeWireDriver: connected to PipeWire, watching for a TAI-clocked graph driver");
    return true;
#endif
}

void PipeWireDriver::stop() {
#ifdef WITH_PIPEWIRE
    if (pw_loop_)
        pw_main_loop_quit(static_cast<struct pw_main_loop*>(pw_loop_));
    if (loop_thread_.joinable())
        loop_thread_.join();

    if (registry_ctx_) {
        delete static_cast<RegistryCtx*>(registry_ctx_);
        registry_ctx_ = nullptr;
    }
    pw_registry_ = nullptr;  /* owned by core, torn down with it below */

    if (pw_core_)    { pw_core_disconnect(static_cast<struct pw_core*>(pw_core_)); pw_core_ = nullptr; }
    if (pw_context_) { pw_context_destroy(static_cast<struct pw_context*>(pw_context_)); pw_context_ = nullptr; }
    if (pw_loop_)    { pw_main_loop_destroy(static_cast<struct pw_main_loop*>(pw_loop_)); pw_loop_ = nullptr; }
#endif
}

int64_t PipeWireDriver::on_ptp_update(int64_t /*offset_ns*/, int64_t /*freq_ppb*/, bool locked) {
    if (pw_core_ && locked && !was_locked_ && !tai_driver_found_.load())
        std::fputs("PipeWireDriver: PTP locked, but no TAI-clocked PipeWire graph driver "
                   "found yet — check pipewire-aes67.conf (clock.id = tai)\n", stderr);
    was_locked_ = locked;
    return 0;
}
