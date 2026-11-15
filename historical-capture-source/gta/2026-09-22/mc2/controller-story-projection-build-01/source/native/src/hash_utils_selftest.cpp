#include "hash_utils.hpp"

#include <windows.h>

#include <array>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <string>

namespace {

struct HashResult {
    std::string path;
    std::string digest;
    std::string error;
    bool ok{};
};

DWORD WINAPI hash_on_small_stack(void* context) {
    auto& result = *static_cast<HashResult*>(context);
    result.ok = diogenes::gta::sha256_file(
        result.path, result.digest, result.error);
    return result.ok ? 0 : 1;
}

}  // namespace

int main() {
    std::array<wchar_t, MAX_PATH> temp_root{};
    if (GetTempPathW(static_cast<DWORD>(temp_root.size()), temp_root.data()) == 0) {
        std::cerr << "GetTempPathW failed\n";
        return 1;
    }
    const auto path = std::filesystem::path(temp_root.data()) /
        L"diogenes-gta-hash-small-stack.bin";
    {
        std::ofstream output(path, std::ios::binary | std::ios::trunc);
        if (!output) {
            std::cerr << "cannot create hash fixture\n";
            return 1;
        }
        std::array<char, 4096> block{};
        for (int i = 0; i < 512; ++i) {
            output.write(block.data(), static_cast<std::streamsize>(block.size()));
        }
        if (!output) {
            std::cerr << "cannot write hash fixture\n";
            return 1;
        }
    }

    HashResult result{path.u8string()};
    HANDLE thread = CreateThread(
        nullptr, 256 * 1024, hash_on_small_stack, &result,
        STACK_SIZE_PARAM_IS_A_RESERVATION, nullptr);
    if (thread == nullptr) {
        std::cerr << "CreateThread failed\n";
        std::filesystem::remove(path);
        return 1;
    }
    const DWORD wait = WaitForSingleObject(thread, 30'000);
    DWORD exit_code = 1;
    const bool exit_ok = wait == WAIT_OBJECT_0 &&
        GetExitCodeThread(thread, &exit_code) && exit_code == 0;
    CloseHandle(thread);
    std::filesystem::remove(path);

    if (!exit_ok || !result.ok || result.digest.size() != 64) {
        std::cerr << "small-stack sha256 failed: " << result.error << '\n';
        return 1;
    }
    return 0;
}
