#pragma once

#ifdef _WIN32
#include <windows.h>
#endif

#include <string>

namespace diogenes::gta {

class ControllerCore;

class PipeServer {
public:
    explicit PipeServer(std::wstring pipe_name);
    ~PipeServer();
    PipeServer(const PipeServer&) = delete;
    PipeServer& operator=(const PipeServer&) = delete;

    bool open();
    void poll(ControllerCore& controller);
    const std::string& last_error() const noexcept { return last_error_; }

private:
    void disconnect();
    std::wstring pipe_name_;
    std::string pending_;
    std::string last_error_;
#ifdef _WIN32
    HANDLE pipe_{INVALID_HANDLE_VALUE};
    bool connected_{false};
#endif
};

}  // namespace diogenes::gta
