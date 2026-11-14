#include "game_adapter.hpp"

#include <cstdint>
#include <string>

namespace diogenes::gta {
namespace {

class MockGameAdapter final : public GameAdapter {
public:
    void set_runtime_generation(std::uint64_t generation) override {
        runtime_generation_ = generation;
    }

    AdapterResult hello() override {
        return AdapterResult::success(
            "{\"backend\":\"native-mock\",\"framework_ready\":true,"
            "\"adapter_ready\":true,\"game_connected\":false,"
            "\"story_mode\":true,\"online_session\":false,\"mock\":true,"
            "\"process_id\":0,\"controller_generation\":" +
            std::to_string(runtime_generation_) +
            ",\"record_binding_ready\":false,\"master_replay_sha256\":null}");
    }

    AdapterResult probe_state() override {
        return AdapterResult::success(
            "{\"story_mode\":true,\"mock\":true,"
            "\"player_position\":[0,0,0],\"camera_position\":[0,0,0],"
            "\"process_id\":0,\"controller_generation\":" +
            std::to_string(runtime_generation_) +
            ",\"record_binding_ready\":false,\"master_replay_sha256\":null}");
    }

    AdapterResult story_projection_sample(const std::string&, const std::string&,
                                          const std::string&) override {
        return AdapterResult::failure(
            "native mock cannot measure game-rendered calibration pixels");
    }

    AdapterResult apply_scenario(const std::string& path,
                                 const std::string& sha256) override {
        if (path.empty() || sha256.empty()) {
            return AdapterResult::failure("scenario path/hash missing");
        }
        return AdapterResult::success("{\"applied\":true,\"mock\":true}");
    }

    AdapterResult record_master(const std::string&, double) override {
        return AdapterResult::failure(
            "native mock does not create replay files; use the Python dry-run backend");
    }

    AdapterResult arm_export_lane(int, const std::string&, const std::string&,
                                  const std::string&, const std::string&,
                                  const std::string&, const std::string&,
                                  const std::string&) override {
        return AdapterResult::failure(
            "native mock cannot arm Rockstar Editor exports");
    }

    AdapterResult export_lane(int, const std::string&, const std::string&,
                              const std::string&, const std::string&,
                              const std::string&, const std::string&,
                              const std::string&) override {
        return AdapterResult::failure(
            "native mock does not export frames; use the Python dry-run backend");
    }

    AdapterResult activate_editor() override {
        return AdapterResult::failure(
            "native mock cannot activate Rockstar Editor");
    }

    AdapterResult editor_input(int) override {
        return AdapterResult::failure(
            "native mock cannot drive Rockstar Editor input");
    }

    AdapterResult probe_editor() override {
        return AdapterResult::failure(
            "native mock has no Rockstar Editor runtime identity");
    }

    AdapterResult capture_audio(const std::string&, const std::string&,
                                const std::string&, double) override {
        return AdapterResult::failure(
            "native mock does not capture audio; use the Python dry-run backend");
    }

    void on_tick() override {}
    void on_present() override {}

private:
    std::uint64_t runtime_generation_{};
};

}  // namespace

std::unique_ptr<GameAdapter> make_mock_game_adapter() {
    return std::make_unique<MockGameAdapter>();
}

}  // namespace diogenes::gta
