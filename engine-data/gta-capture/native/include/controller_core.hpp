#pragma once

#include <string>

#include "game_adapter.hpp"

namespace diogenes::gta {

class ControllerCore {
public:
    explicit ControllerCore(GameAdapter& adapter) : adapter_(adapter) {}

    std::string handle_line(const std::string& line);
    bool detach_requested() const noexcept { return detach_requested_; }

private:
    GameAdapter& adapter_;
    bool detach_requested_{false};
};

std::string json_escape(const std::string& value);

}  // namespace diogenes::gta
