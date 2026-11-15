#pragma once

#include <atomic>
#include <cstdint>
#include <memory>

#include "controller_core.hpp"
#include "game_adapter.hpp"

namespace diogenes::gta {

// Owns the adapter/controller for the lifetime of the GTA process. ScriptHook may
// launch ScriptMain more than once while entering Rockstar Editor; those launches
// must not discard the master replay binding held by the adapter.
class ProcessRuntime {
public:
    explicit ProcessRuntime(std::unique_ptr<GameAdapter> adapter);

    GameAdapter& adapter() noexcept { return *adapter_; }
    ControllerCore& controller() noexcept { return controller_; }

    std::uint64_t begin_generation();
    std::uint64_t active_generation() const noexcept;
    bool is_active(std::uint64_t generation) const noexcept;

private:
    std::unique_ptr<GameAdapter> adapter_;
    ControllerCore controller_;
    std::atomic<std::uint64_t> generation_{0};
};

}  // namespace diogenes::gta
