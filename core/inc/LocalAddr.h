//
// LocalAddr.h - discover a non-loopback IPv4 address for direct-connect candidates
//
#pragma once

#include <ifaddrs.h>
#include <arpa/inet.h>
#include <net/if.h>
#include <string>

namespace nebula {

// Returns the first usable non-loopback IPv4 address, or empty if none.
inline std::string LocalIPv4() {
    struct ifaddrs* ifaddr = nullptr;
    if (getifaddrs(&ifaddr) != 0) return {};
    std::string result;
    for (auto* ifa = ifaddr; ifa; ifa = ifa->ifa_next) {
        if (!ifa->ifa_addr) continue;
        if (ifa->ifa_addr->sa_family != AF_INET) continue;
        if (!(ifa->ifa_flags & IFF_UP)) continue;
        if (ifa->ifa_flags & IFF_LOOPBACK) continue;
        char buf[INET_ADDRSTRLEN] = {0};
        auto* sin = (struct sockaddr_in*)ifa->ifa_addr;
        inet_ntop(AF_INET, &sin->sin_addr, buf, sizeof(buf));
        std::string ip = buf;
        if (ip.rfind("169.254.", 0) == 0) continue; // skip link-local
        result = ip;
        break;
    }
    freeifaddrs(ifaddr);
    return result;
}

} // namespace nebula
