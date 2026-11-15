#include "pipe_server.hpp"

#include "controller_core.hpp"

#include <algorithm>
#include <array>

namespace diogenes::gta {

PipeServer::PipeServer(std::wstring pipe_name) : pipe_name_(std::move(pipe_name)) {}

PipeServer::~PipeServer() { disconnect(); }

bool PipeServer::open() {
    disconnect();
    pipe_ = CreateNamedPipeW(
        pipe_name_.c_str(), PIPE_ACCESS_DUPLEX,
        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT,
        1, 64 * 1024, 64 * 1024, 0, nullptr);
    if (pipe_ == INVALID_HANDLE_VALUE) {
        last_error_ = "CreateNamedPipeW failed: " + std::to_string(GetLastError());
        return false;
    }
    connected_ = false;
    pending_.clear();
    last_error_.clear();
    return true;
}

void PipeServer::disconnect() {
    if (pipe_ != INVALID_HANDLE_VALUE) {
        if (connected_) FlushFileBuffers(pipe_);
        DisconnectNamedPipe(pipe_);
        CloseHandle(pipe_);
        pipe_ = INVALID_HANDLE_VALUE;
    }
    connected_ = false;
}

void PipeServer::poll(ControllerCore& controller) {
    if (pipe_ == INVALID_HANDLE_VALUE && !open()) return;
    if (!connected_) {
        if (ConnectNamedPipe(pipe_, nullptr)) {
            connected_ = true;
        } else {
            const auto error = GetLastError();
            if (error == ERROR_PIPE_CONNECTED) {
                connected_ = true;
            } else if (error == ERROR_PIPE_LISTENING) {
                return;
            } else if (error == ERROR_NO_DATA) {
                // NOWAIT semantics: a client connected and disconnected before
                // we observed it. The instance stays dead until it is reset;
                // returning here would leave the pipe unusable forever.
                open();
                return;
            } else {
                last_error_ = "ConnectNamedPipe failed: " + std::to_string(error);
                open();
                return;
            }
        }
    }

    DWORD available = 0;
    if (!PeekNamedPipe(pipe_, nullptr, 0, nullptr, &available, nullptr)) {
        const auto error = GetLastError();
        if (error == ERROR_BROKEN_PIPE || error == ERROR_NO_DATA) open();
        return;
    }
    if (available == 0) return;

    std::array<char, 4096> buffer{};
    DWORD read = 0;
    if (!ReadFile(pipe_, buffer.data(),
                  static_cast<DWORD>(std::min<std::size_t>(buffer.size(), available)),
                  &read, nullptr)) {
        open();
        return;
    }
    pending_.append(buffer.data(), read);
    while (true) {
        const auto newline = pending_.find('\n');
        if (newline == std::string::npos) break;
        const auto line = pending_.substr(0, newline);
        pending_.erase(0, newline + 1);
        const auto response = controller.handle_line(line);
        DWORD written = 0;
        if (!WriteFile(pipe_, response.data(), static_cast<DWORD>(response.size()),
                       &written, nullptr) || written != response.size()) {
            open();
            return;
        }
    }
}

}  // namespace diogenes::gta
