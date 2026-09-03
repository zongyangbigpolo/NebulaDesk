//
// main.mm - session viewer entry point (independent high-refresh process)
//
// Launched by the manager. Non-sensitive args via argv; the shared secret comes
// from the environment (NEBULA_PSK) so it never appears in `ps`. Lifecycle status
// is reported to stdout via NEBULA_STATUS lines (see SessionStatus.h).
//
#import <Cocoa/Cocoa.h>
#include "AppDelegate.h"
#include "NebulaLog.h"

#include <cstdlib>
#include <string>

int main(int argc, const char* argv[]) {
    @autoreleasepool {
        nebula::LogInit("com.nebula.session");

        std::string host = "127.0.0.1";
        uint16_t port = 7000;
        std::string title = "Nebula session";
        std::string relayHost, deviceId, token;
        uint16_t relayPort = 7100;
        for (int i = 1; i < argc; ++i) {
            std::string a = argv[i];
            auto next = [&](const char* def) { return (i + 1 < argc) ? argv[++i] : def; };
            if (a == "--host")       host = next("127.0.0.1");
            else if (a == "--port")  port = (uint16_t)atoi(next("7000"));
            else if (a == "--title") title = next("Nebula session");
            else if (a == "--relay") relayHost = next("");
            else if (a == "--relay-port") relayPort = (uint16_t)atoi(next("7100"));
            else if (a == "--device") deviceId = next("");
            else if (a == "--token")  token = next("");
        }

        // Sensitive secret via environment (not argv).
        std::string psk = "nebula-default-psk";
        if (const char* envPsk = getenv("NEBULA_PSK")) psk = envPsk;
        if (const char* envToken = getenv("NEBULA_RELAY_TOKEN")) token = envToken;

        NSApplication* app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];

        NebulaAppDelegate* delegate = [[NebulaAppDelegate alloc] init];
        delegate.host = host;
        delegate.port = port;
        delegate.psk = psk;
        delegate.title = title;
        delegate.relayHost = relayHost;
        delegate.relayPort = relayPort;
        delegate.deviceId = deviceId;
        delegate.token = token;
        [app setDelegate:delegate];

        NEBULA_LOGI("session", "connecting to %s:%u (%s)", host.c_str(), port, title.c_str());
        [app run];
    }
    return 0;
}
