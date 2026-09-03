//
// NebulaLog.cpp - os_log backed logging implementation
//
#include "NebulaLog.h"

#include <os/log.h>
#include <cstdarg>
#include <cstdio>

namespace nebula {

static os_log_t g_log = OS_LOG_DEFAULT;

void LogInit(const char* subsystem) {
    g_log = os_log_create(subsystem, "nebula");
}

void LogWrite(LogLevel level, const char* tag, const char* fmt, ...) {
    char body[1024];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(body, sizeof(body), fmt, ap);
    va_end(ap);

    os_log_type_t t = OS_LOG_TYPE_DEFAULT;
    switch (level) {
        case LogLevel::Debug: t = OS_LOG_TYPE_DEBUG; break;
        case LogLevel::Info:  t = OS_LOG_TYPE_INFO;  break;
        case LogLevel::Warn:  t = OS_LOG_TYPE_DEFAULT; break;
        case LogLevel::Error: t = OS_LOG_TYPE_ERROR; break;
    }
    os_log_with_type(g_log, t, "[%{public}s] %{public}s", tag, body);
    // Also mirror to stderr for CLI dev runs.
    fprintf(stderr, "[%s] %s\n", tag, body);
}

} // namespace nebula
