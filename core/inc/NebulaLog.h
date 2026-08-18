//
// NebulaLog.h - lightweight logging (os_log backed) for Nebula
//
#pragma once

#include <string>

namespace nebula {

enum class LogLevel { Debug, Info, Warn, Error };

void LogInit(const char* subsystem);
void LogWrite(LogLevel level, const char* tag, const char* fmt, ...) __attribute__((format(printf, 3, 4)));

} // namespace nebula

#define NEBULA_LOGD(tag, ...) ::nebula::LogWrite(::nebula::LogLevel::Debug, tag, __VA_ARGS__)
#define NEBULA_LOGI(tag, ...) ::nebula::LogWrite(::nebula::LogLevel::Info,  tag, __VA_ARGS__)
#define NEBULA_LOGW(tag, ...) ::nebula::LogWrite(::nebula::LogLevel::Warn,  tag, __VA_ARGS__)
#define NEBULA_LOGE(tag, ...) ::nebula::LogWrite(::nebula::LogLevel::Error, tag, __VA_ARGS__)
