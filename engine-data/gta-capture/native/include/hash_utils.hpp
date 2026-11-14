#pragma once

#include <cstddef>
#include <cstdint>
#include <string>

namespace diogenes::gta {

bool sha256_file(const std::string& path, std::string& digest, std::string& error);
bool sha256_bytes(const void* data, std::size_t size, std::string& digest,
                  std::string& error);

}  // namespace diogenes::gta
