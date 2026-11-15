#include "controller_core.hpp"
#include "game_adapter.hpp"
#include "process_runtime.hpp"

#include <iostream>
#include <string>

int main(int argc, char** argv) {
    diogenes::gta::ProcessRuntime runtime(diogenes::gta::make_mock_game_adapter());
    const auto generation = runtime.begin_generation();
    auto& controller = runtime.controller();
    if (argc == 2 && std::string(argv[1]) == "--self-test") {
        const auto response = controller.handle_line(
            "{\"schema\":\"dm-gta-control/1\",\"id\":\"selftest\","
            "\"command\":\"hello\",\"payload\":{}}");
        if (response.find("\"ok\":true") == std::string::npos ||
            response.find("\"adapter_ready\":true") == std::string::npos) {
            std::cerr << response;
            return 1;
        }
        if (response.find("\"controller_generation\":" +
                          std::to_string(generation)) == std::string::npos ||
            response.find("\"record_binding_ready\":false") == std::string::npos) {
            std::cerr << response;
            return 1;
        }
        // Python sends ensure_ascii JSON: non-ASCII arrives as \uXXXX (with
        // surrogate pairs) and must round-trip as UTF-8 bytes.
        const auto unicode = controller.handle_line(
            "{\"schema\":\"dm-gta-control/1\","
            "\"id\":\"caf\\u00e9-\\u738b-\\ud83c\\udfae\","
            "\"command\":\"hello\",\"payload\":{}}");
        if (unicode.find("\"id\":\"caf\xC3\xA9-\xE7\x8E\x8B-\xF0\x9F\x8E\xAE\"")
                == std::string::npos) {
            std::cerr << unicode;
            return 1;
        }
        // A lone high surrogate must fail closed, not decode to garbage.
        const auto malformed = controller.handle_line(
            "{\"schema\":\"dm-gta-control/1\",\"id\":\"bad\\ud800\","
            "\"command\":\"hello\",\"payload\":{}}");
        if (malformed.find("request id is missing") == std::string::npos) {
            std::cerr << malformed;
            return 1;
        }
        const auto probe = controller.handle_line(
            "{\"schema\":\"dm-gta-control/1\",\"id\":\"selftest-probe\","
            "\"command\":\"probe_state\",\"payload\":{}}");
        if (probe.find("\"ok\":true") == std::string::npos ||
            probe.find("\"controller_generation\":" +
                       std::to_string(generation)) == std::string::npos ||
            probe.find("\"record_binding_ready\":false") == std::string::npos) {
            std::cerr << probe;
            return 1;
        }
        return 0;
    }

    std::string line;
    while (std::getline(std::cin, line)) {
        std::cout << controller.handle_line(line) << std::flush;
        if (controller.detach_requested()) break;
    }
    return 0;
}
