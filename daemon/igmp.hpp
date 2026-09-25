//
//  igmp.hpp
//
//  Copyright (c) 2019 2020 Andrea Bondavalli. All rights reserved.
//
//  This program is free software: you can redistribute it and/or modify
//  it under the terms of the GNU General Public License as published by
//  the Free Software Foundation, either version 3 of the License, or
//  any later version.
//
//  This program is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//  GNU General Public License for more details.
//
//  You should have received a copy of the GNU General Public License
//  along with this program.  If not, see <http://www.gnu.org/licenses/>.
//

#ifndef _IGMP_HPP_
#define _IGMP_HPP_

#include <boost/asio.hpp>
#include <map>
#include <memory>
#include <unordered_map>
#include <mutex>

#include "log.hpp"

using namespace boost::asio;
using namespace boost::asio::ip;
using namespace boost::system;

// One socket per multicast group: Linux caps memberships per socket (net.ipv4.igmp_max_memberships,
// default 20), and a single shared socket hit that cap with 16 tx + 16 rx streams - further joins
// failed with ENOBUFS and the NIC never passed those groups (a connected Sink received nothing).
class IGMP {
 public:
  bool join(const std::string& interface_ip, const std::string& mcast_ip) {
    if (interface_ip.empty() || mcast_ip.empty()) {
      return false;
    }
    error_code ec;
    auto mcast = ip::make_address(mcast_ip, ec);
    auto iface = ip::make_address(interface_ip, ec);
    if (ec || !mcast.is_v4() || !iface.is_v4()) {
      return false;
    }
    uint32_t key = mcast.to_v4().to_uint();
    std::lock_guard<std::mutex> lock{mutex};
    auto it = groups_.find(key);
    if (it != groups_.end()) {
      it->second.ref++;
      return true;
    }
    auto socket = std::make_unique<udp::socket>(io_service_);
    socket->open(udp::v4(), ec);
    if (!ec) socket->set_option(udp::socket::reuse_address(true), ec);
    if (!ec) socket->bind(udp::endpoint(address_v4::any(), 0), ec);
    if (!ec) socket->set_option(ip::multicast::join_group(mcast.to_v4(), iface.to_v4()), ec);
    if (ec) {
      BOOST_LOG_TRIVIAL(error) << "igmp:: failed to joined multicast group "
                               << mcast_ip << " " << ec.message();
      return false;
    }
    socket->set_option(ip::multicast::enable_loopback(true), ec);
    if (ec) {
      BOOST_LOG_TRIVIAL(error)
          << "igmp:: enable loopback option " << ec.message();
    }
    BOOST_LOG_TRIVIAL(info) << "igmp:: joined multicast group " << mcast_ip
                            << " on " << interface_ip;
    groups_.emplace(key, Group{1, std::move(socket)});
    return true;
  }

  bool leave(const std::string& interface_ip, const std::string& mcast_ip) {
    if (interface_ip.empty() || mcast_ip.empty()) {
      return false;
    }
    error_code ec;
    auto mcast = ip::make_address(mcast_ip, ec);
    auto iface = ip::make_address(interface_ip, ec);
    if (ec || !mcast.is_v4() || !iface.is_v4()) {
      return false;
    }
    std::lock_guard<std::mutex> lock{mutex};
    auto it = groups_.find(mcast.to_v4().to_uint());
    if (it == groups_.end()) {
      return false;
    }
    if (--it->second.ref > 0) {
      return true;
    }
    it->second.socket->set_option(ip::multicast::leave_group(mcast.to_v4(), iface.to_v4()), ec);
    if (ec) {
      BOOST_LOG_TRIVIAL(error) << "igmp:: failed to leave multicast group "
                               << mcast_ip << " " << ec.message();
    } else {
      BOOST_LOG_TRIVIAL(info)
          << "igmp:: left multicast group " << mcast_ip << " on " << interface_ip;
    }
    groups_.erase(it);  // closes the socket - the kernel drops the membership either way
    return !ec;
  }

 private:
  struct Group {
    int ref;
    std::unique_ptr<udp::socket> socket;
  };
#if BOOST_VERSION < 108700
  io_service io_service_;
#else
  io_context io_service_;
#endif
  std::unordered_map<uint32_t, Group> groups_;
  std::mutex mutex;
};
#endif
