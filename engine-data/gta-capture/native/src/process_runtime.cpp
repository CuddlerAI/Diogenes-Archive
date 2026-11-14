#include "process_runtime.hpp"

#include <stdexcept>
#include <utility>

namespace diogenes::gta {

ProcessRuntime::ProcessRuntime(std::unique_ptr<GameAdapter> adapter)
    : adapter_(std::move(adapter)),
      controller_(adapter_ ? *adapter_ : throw std::invalid_argument("adapter is required")) {}

std::uint64_t ProcessRuntime::begin_generation() {
    auto generation = generation_.fetch_add(1, std::memory_order_acq_rel) + 1;
    if (generation == 0) {
        // Wrapping would make a stale fiber appear current. This is practically
        // unreachable, but fail closed by reserving zero and publishing one.
        generation_.store(1, std::memory_order_release);
        generation = 1;
    }
    adapter_->set_runtime_generation(generation);
    return generation;
}

std::uint64_t ProcessRuntime::active_generation() const noexcept {
    return generation_.load(std::memory_order_acquire);
}

bool ProcessRuntime::is_active(std::uint64_t generation) const noexcept {
    return generation != 0 && active_generation() == generation;
}

}  // namespace diogenes::gta
