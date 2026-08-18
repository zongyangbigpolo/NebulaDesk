//
// ProcessSpawner.mm - posix_spawn implementation (macOS)
//
#include "IProcessSpawner.h"
#include "NebulaLog.h"

#include <spawn.h>
#include <signal.h>
#include <sys/wait.h>
#include <unistd.h>
#include <cstring>

extern char** environ;

#define TAG "spawn"

namespace nebula {
namespace {

class PosixProcessSpawner final : public IProcessSpawner {
public:
    SpawnedProcess spawn(const SpawnRequest& req) override {
        SpawnedProcess proc;

        // argv: executable + args + NULL
        std::vector<std::string> argvStore;
        argvStore.push_back(req.executable);
        for (auto& a : req.args) argvStore.push_back(a);
        std::vector<char*> argv;
        for (auto& s : argvStore) argv.push_back(const_cast<char*>(s.c_str()));
        argv.push_back(nullptr);

        // envp: inherit current environment + the request's sensitive overrides.
        std::vector<std::string> envStore;
        for (char** e = environ; e && *e; ++e) envStore.push_back(*e);
        for (auto& kv : req.env) envStore.push_back(kv.first + "=" + kv.second);
        std::vector<char*> envp;
        for (auto& s : envStore) envp.push_back(const_cast<char*>(s.c_str()));
        envp.push_back(nullptr);

        pid_t pid = 0;
        int rc = posix_spawn(&pid, req.executable.c_str(), nullptr, nullptr,
                             argv.data(), envp.data());
        if (rc != 0) {
            NEBULA_LOGE(TAG, "posix_spawn failed for %s: %s",
                      req.executable.c_str(), strerror(rc));
            return proc;
        }
        proc.pid = pid;
        NEBULA_LOGI(TAG, "spawned %s pid=%lld", req.executable.c_str(), (long long)pid);
        return proc;
    }

    bool isAlive(const SpawnedProcess& p) override {
        if (p.pid <= 0) return false;
        // Reap if exited; kill(pid,0) probes existence.
        int status = 0;
        pid_t r = waitpid((pid_t)p.pid, &status, WNOHANG);
        if (r == (pid_t)p.pid) return false;     // exited and reaped
        return kill((pid_t)p.pid, 0) == 0;        // still alive
    }

    void terminate(const SpawnedProcess& p) override {
        if (p.pid > 0) {
            kill((pid_t)p.pid, SIGTERM);
            int status = 0;
            waitpid((pid_t)p.pid, &status, WNOHANG);
        }
    }
};

} // namespace

std::unique_ptr<IProcessSpawner> CreateProcessSpawner() {
    return std::make_unique<PosixProcessSpawner>();
}

} // namespace nebula
