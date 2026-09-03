//
// ProcessSpawner.h - platform-neutral child-process launch (manager side)
//
// The Flutter manager uses this to spawn an independent session process
// (nebula_session). macOS/Linux backend = posix_spawn; Windows = CreateProcess.
// Sensitive args (tokens/keys) are passed via environment, never argv.
//
#pragma once

#include <cstdint>
#include <map>
#include <memory>
#include <string>
#include <vector>

namespace nebula {

struct SpawnRequest {
    std::string executable;                  // path to nebula_session
    std::vector<std::string> args;           // non-sensitive args
    std::map<std::string, std::string> env;  // sensitive values (token/key)
};

// Opaque handle to a spawned process.
struct SpawnedProcess {
    int64_t pid = -1;
    bool running() const { return pid > 0; }
};

class IProcessSpawner {
public:
    virtual ~IProcessSpawner() = default;
    virtual SpawnedProcess spawn(const SpawnRequest& req) = 0;
    virtual bool isAlive(const SpawnedProcess& p) = 0;
    virtual void terminate(const SpawnedProcess& p) = 0;
};

std::unique_ptr<IProcessSpawner> CreateProcessSpawner();

} // namespace nebula
