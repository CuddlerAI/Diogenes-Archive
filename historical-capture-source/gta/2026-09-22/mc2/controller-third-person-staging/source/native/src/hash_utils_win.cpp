#include "hash_utils.hpp"

#include <windows.h>
#include <bcrypt.h>

#include <array>
#include <filesystem>
#include <fstream>
#include <iomanip>
#include <limits>
#include <sstream>
#include <vector>

namespace diogenes::gta {
namespace {

class AlgorithmHandle {
public:
    ~AlgorithmHandle() {
        if (value_ != nullptr) BCryptCloseAlgorithmProvider(value_, 0);
    }
    BCRYPT_ALG_HANDLE* put() { return &value_; }
    BCRYPT_ALG_HANDLE get() const { return value_; }

private:
    BCRYPT_ALG_HANDLE value_{};
};

class HashHandle {
public:
    ~HashHandle() {
        if (value_ != nullptr) BCryptDestroyHash(value_);
    }
    BCRYPT_HASH_HANDLE* put() { return &value_; }
    BCRYPT_HASH_HANDLE get() const { return value_; }

private:
    BCRYPT_HASH_HANDLE value_{};
};

std::string status_error(const char* operation, NTSTATUS status) {
    std::ostringstream out;
    out << operation << " failed with NTSTATUS 0x" << std::hex
        << static_cast<unsigned long>(status);
    return out.str();
}

std::string hex(const std::array<std::uint8_t, 32>& digest) {
    std::ostringstream out;
    out << std::hex << std::setfill('0');
    for (const auto value : digest) {
        out << std::setw(2) << static_cast<unsigned>(value);
    }
    return out.str();
}

template <typename Feed>
bool sha256(Feed&& feed, std::string& digest, std::string& error) {
    AlgorithmHandle algorithm;
    auto status = BCryptOpenAlgorithmProvider(algorithm.put(), BCRYPT_SHA256_ALGORITHM,
                                              nullptr, 0);
    if (!BCRYPT_SUCCESS(status)) {
        error = status_error("BCryptOpenAlgorithmProvider", status);
        return false;
    }

    DWORD object_length = 0;
    DWORD copied = 0;
    status = BCryptGetProperty(algorithm.get(), BCRYPT_OBJECT_LENGTH,
                               reinterpret_cast<PUCHAR>(&object_length),
                               sizeof(object_length), &copied, 0);
    if (!BCRYPT_SUCCESS(status) || copied != sizeof(object_length)) {
        error = status_error("BCryptGetProperty", status);
        return false;
    }
    std::vector<std::uint8_t> object(object_length);
    HashHandle hash;
    status = BCryptCreateHash(algorithm.get(), hash.put(), object.data(),
                              static_cast<ULONG>(object.size()), nullptr, 0, 0);
    if (!BCRYPT_SUCCESS(status)) {
        error = status_error("BCryptCreateHash", status);
        return false;
    }
    if (!feed(hash.get(), error)) return false;

    std::array<std::uint8_t, 32> raw{};
    status = BCryptFinishHash(hash.get(), raw.data(), static_cast<ULONG>(raw.size()), 0);
    if (!BCRYPT_SUCCESS(status)) {
        error = status_error("BCryptFinishHash", status);
        return false;
    }
    digest = hex(raw);
    error.clear();
    return true;
}

}  // namespace

bool sha256_file(const std::string& path, std::string& digest, std::string& error) {
    std::ifstream input(std::filesystem::u8path(path), std::ios::binary);
    if (!input) {
        error = "cannot open file for sha256: " + path;
        return false;
    }
    return sha256([&input](BCRYPT_HASH_HANDLE hash, std::string& feed_error) {
        // ScriptHook runs ASI scripts on a small fiber stack. A 1 MiB local
        // array exhausts that stack when record_master hashes the newly sealed
        // Rockstar clip. Keep the same bounded I/O size, but allocate it on
        // the heap so hashing is safe from the ScriptHook fiber.
        std::vector<char> block(1024 * 1024);
        while (input) {
            input.read(block.data(), static_cast<std::streamsize>(block.size()));
            const auto count = input.gcount();
            if (count <= 0) break;
            const auto status = BCryptHashData(
                hash, reinterpret_cast<PUCHAR>(block.data()),
                static_cast<ULONG>(count), 0);
            if (!BCRYPT_SUCCESS(status)) {
                feed_error = status_error("BCryptHashData", status);
                return false;
            }
        }
        if (input.bad()) {
            feed_error = "read failed while hashing file";
            return false;
        }
        return true;
    }, digest, error);
}

bool sha256_bytes(const void* data, std::size_t size, std::string& digest,
                  std::string& error) {
    if (size > static_cast<std::size_t>((std::numeric_limits<ULONG>::max)())) {
        error = "sha256 input is too large";
        return false;
    }
    return sha256([data, size](BCRYPT_HASH_HANDLE hash, std::string& feed_error) {
        const auto status = BCryptHashData(
            hash, reinterpret_cast<PUCHAR>(const_cast<void*>(data)),
            static_cast<ULONG>(size), 0);
        if (!BCRYPT_SUCCESS(status)) {
            feed_error = status_error("BCryptHashData", status);
            return false;
        }
        return true;
    }, digest, error);
}

}  // namespace diogenes::gta
