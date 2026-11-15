#include "process_runtime.hpp"

#include <cstdint>
#include <iostream>
#include <memory>
#include <string>

namespace {

class LifecycleAdapter final : public diogenes::gta::GameAdapter {
public:
    void set_runtime_generation(std::uint64_t generation) override {
        generation_ = generation;
    }

    diogenes::gta::AdapterResult hello() override {
        return diogenes::gta::AdapterResult::success(
            "{\"controller_generation\":" + std::to_string(generation_) +
            ",\"record_binding_ready\":" + (bound_ ? "true" : "false") +
            ",\"master_replay_sha256\":" +
            (bound_ ? "\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\""
                    : "null") + "}");
    }

    diogenes::gta::AdapterResult probe_state() override { return hello(); }
    diogenes::gta::AdapterResult apply_scenario(const std::string&,
                                                 const std::string&) override {
        return diogenes::gta::AdapterResult::success();
    }
    diogenes::gta::AdapterResult record_master(const std::string&, double) override {
        bound_ = true;
        return diogenes::gta::AdapterResult::success();
    }
    diogenes::gta::AdapterResult arm_export_lane(
            int, const std::string&, const std::string&, const std::string&,
            const std::string&, const std::string&, const std::string&,
            const std::string&) override {
        return bound_ ? diogenes::gta::AdapterResult::success()
                      : diogenes::gta::AdapterResult::failure("master not bound");
    }
    diogenes::gta::AdapterResult export_lane(int, const std::string&,
                                              const std::string&,
                                              const std::string&,
                                              const std::string&,
                                              const std::string&,
                                              const std::string&,
                                              const std::string&) override {
        return bound_ ? diogenes::gta::AdapterResult::success()
                      : diogenes::gta::AdapterResult::failure("master not bound");
    }
    diogenes::gta::AdapterResult activate_editor() override {
        return diogenes::gta::AdapterResult::success();
    }
    diogenes::gta::AdapterResult editor_input(int) override {
        return diogenes::gta::AdapterResult::success();
    }
    diogenes::gta::AdapterResult probe_editor() override {
        return diogenes::gta::AdapterResult::success();
    }
    diogenes::gta::AdapterResult capture_audio(const std::string&,
                                                const std::string&,
                                                const std::string&, double) override {
        return diogenes::gta::AdapterResult::success();
    }
    void on_tick() override {}
    void on_present() override {}

private:
    std::uint64_t generation_{};
    bool bound_{false};
};

int fail(const char* message) {
    std::cerr << message << '\n';
    return 1;
}

}  // namespace

int main() {
    auto adapter = std::make_unique<LifecycleAdapter>();
    auto* const original_adapter = adapter.get();
    diogenes::gta::ProcessRuntime runtime(std::move(adapter));
    auto* const original_controller = &runtime.controller();

    const auto first = runtime.begin_generation();
    if (first == 0 || !runtime.is_active(first)) return fail("first generation inactive");
    if (!runtime.adapter().record_master("ignored", 1.0).ok) {
        return fail("test binding failed");
    }

    const auto second = runtime.begin_generation();
    if (second <= first || runtime.is_active(first) || !runtime.is_active(second)) {
        return fail("generation did not retire the old fiber");
    }
    if (&runtime.adapter() != original_adapter ||
        &runtime.controller() != original_controller) {
        return fail("adapter/controller were replaced across relaunch");
    }

    const auto probe = runtime.adapter().hello();
    if (!probe.ok ||
        probe.payload_json.find("\"controller_generation\":" +
                                std::to_string(second)) == std::string::npos ||
        probe.payload_json.find("\"record_binding_ready\":true") ==
            std::string::npos) {
        return fail("master binding did not survive relaunch");
    }
    return 0;
}
