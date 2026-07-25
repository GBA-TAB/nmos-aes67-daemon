#include "ravenna_ptp.hpp"

#include <cerrno>
#include <cstring>
#include <cstdio>
#include <unistd.h>
#include <sys/socket.h>
#include <linux/netlink.h>

/* LKM headers — adjust include path via CMake */
#include "MT_ALSA_message_defs.h"
#include "audio_streamer_clock_PTP_defs.h"

static constexpr int NETLINK_U2K = NETLINK_U2K_ID;  /* 31 */
static constexpr int RECV_TIMEOUT_MS = 500;

RavennaPtp::RavennaPtp() = default;

RavennaPtp::~RavennaPtp() {
    close();
}

bool RavennaPtp::open() {
    fd_ = ::socket(PF_NETLINK, SOCK_RAW, NETLINK_U2K);
    if (fd_ < 0) {
        std::perror("RavennaPtp: socket");
        return false;
    }

    struct sockaddr_nl addr{};
    addr.nl_family = AF_NETLINK;
    addr.nl_pid    = static_cast<uint32_t>(getpid());
    addr.nl_groups = 0;

    if (::bind(fd_, reinterpret_cast<struct sockaddr*>(&addr), sizeof(addr)) < 0) {
        std::perror("RavennaPtp: bind");
        ::close(fd_);
        fd_ = -1;
        return false;
    }

    /* Apply receive timeout so get_status() doesn't block indefinitely */
    struct timeval tv{ 0, RECV_TIMEOUT_MS * 1000 };
    ::setsockopt(fd_, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    return true;
}

void RavennaPtp::close() {
    if (fd_ >= 0) {
        ::close(fd_);
        fd_ = -1;
    }
}

std::optional<RavennaPtpStatus> RavennaPtp::get_status() {
    if (fd_ < 0) return std::nullopt;

    /* Build request: nlmsghdr + MT_ALSA_msg (no payload for GetPTPStatus) */
    struct {
        struct nlmsghdr nlh;
        struct MT_ALSA_msg msg;
    } req{};

    req.nlh.nlmsg_len   = sizeof(req);
    req.nlh.nlmsg_type  = NLMSG_DONE;
    req.nlh.nlmsg_flags = 0;
    req.nlh.nlmsg_seq   = 0;
    req.nlh.nlmsg_pid   = static_cast<uint32_t>(getpid());

    req.msg.id       = MT_ALSA_Msg_GetPTPStatus;
    req.msg.errCode  = 0;
    req.msg.dataSize = 0;
    req.msg.data     = nullptr;

    struct sockaddr_nl dest{};
    dest.nl_family = AF_NETLINK;
    dest.nl_pid    = 0;  /* kernel */
    dest.nl_groups = 0;

    ssize_t sent = ::sendto(fd_, &req, sizeof(req), 0,
                            reinterpret_cast<struct sockaddr*>(&dest), sizeof(dest));
    if (sent < 0) {
        std::perror("RavennaPtp: sendto");
        return std::nullopt;
    }

    /* Receive response */
    alignas(alignof(struct nlmsghdr)) char buf[NLMSG_SPACE(MAX_PAYLOAD)];
    struct sockaddr_nl src{};
    socklen_t src_len = sizeof(src);

    ssize_t n = ::recvfrom(fd_, buf, sizeof(buf), 0,
                           reinterpret_cast<struct sockaddr*>(&src), &src_len);
    if (n < 0) {
        if (errno != EAGAIN && errno != EWOULDBLOCK)
            std::perror("RavennaPtp: recvfrom");
        return std::nullopt;
    }

    if (n < static_cast<ssize_t>(NLMSG_HDRLEN + sizeof(MT_ALSA_msg))) {
        std::fprintf(stderr, "RavennaPtp: response too short (%zd bytes)\n", n);
        return std::nullopt;
    }

    const auto* nlh = reinterpret_cast<const struct nlmsghdr*>(buf);
    const auto* msg = reinterpret_cast<const MT_ALSA_msg*>(NLMSG_DATA(nlh));

    if (msg->id != MT_ALSA_Msg_GetPTPStatus || msg->errCode != 0)
        return std::nullopt;

    if (msg->dataSize < static_cast<int>(sizeof(TPTPStatus)))
        return std::nullopt;

    const auto* ptp = reinterpret_cast<const TPTPStatus*>(
        reinterpret_cast<const char*>(msg) + sizeof(MT_ALSA_msg));

    RavennaPtpStatus out{};
    out.lock_state      = static_cast<int>(ptp->nPTPLockStatus);
    out.gmid[0]         = ptp->ui64GMID[0];
    out.gmid[1]         = ptp->ui64GMID[1];
    out.network_jitter  = ptp->i32NetworkJitter;
    out.clock_jitter    = ptp->i32ClockJitter;
    return out;
}
