#include "controller_core.hpp"

#include <cctype>
#include <cstdlib>
#include <sstream>
#include <string_view>

namespace diogenes::gta {
namespace {

constexpr std::string_view kSchema = "dm-gta-control/1";

bool parse_hex4(const std::string& text, std::size_t pos, unsigned& value) {
    if (pos + 4 > text.size()) return false;
    value = 0;
    for (std::size_t i = 0; i < 4; ++i) {
        const char ch = text[pos + i];
        value <<= 4;
        if (ch >= '0' && ch <= '9') value |= static_cast<unsigned>(ch - '0');
        else if (ch >= 'a' && ch <= 'f') value |= static_cast<unsigned>(ch - 'a' + 10);
        else if (ch >= 'A' && ch <= 'F') value |= static_cast<unsigned>(ch - 'A' + 10);
        else return false;
    }
    return true;
}

void append_utf8(std::string& out, unsigned code) {
    if (code < 0x80) {
        out.push_back(static_cast<char>(code));
    } else if (code < 0x800) {
        out.push_back(static_cast<char>(0xC0 | (code >> 6)));
        out.push_back(static_cast<char>(0x80 | (code & 0x3F)));
    } else if (code < 0x10000) {
        out.push_back(static_cast<char>(0xE0 | (code >> 12)));
        out.push_back(static_cast<char>(0x80 | ((code >> 6) & 0x3F)));
        out.push_back(static_cast<char>(0x80 | (code & 0x3F)));
    } else {
        out.push_back(static_cast<char>(0xF0 | (code >> 18)));
        out.push_back(static_cast<char>(0x80 | ((code >> 12) & 0x3F)));
        out.push_back(static_cast<char>(0x80 | ((code >> 6) & 0x3F)));
        out.push_back(static_cast<char>(0x80 | (code & 0x3F)));
    }
}

std::string find_string(const std::string& json, const std::string& key) {
    const auto marker = std::string{"\""} + key + "\"";
    auto pos = json.find(marker);
    if (pos == std::string::npos) return {};
    pos = json.find(':', pos + marker.size());
    if (pos == std::string::npos) return {};
    pos = json.find('"', pos + 1);
    if (pos == std::string::npos) return {};
    std::string out;
    bool escaped = false;
    for (++pos; pos < json.size(); ++pos) {
        const char ch = json[pos];
        if (escaped) {
            switch (ch) {
                case 'n': out.push_back('\n'); break;
                case 'r': out.push_back('\r'); break;
                case 't': out.push_back('\t'); break;
                case 'u': {
                    // Python sends ensure_ascii JSON, so any non-ASCII path
                    // arrives as \uXXXX; decode (with surrogate pairs) to UTF-8
                    // and fail closed on malformed sequences.
                    unsigned code = 0;
                    if (!parse_hex4(json, pos + 1, code)) return {};
                    pos += 4;
                    if (code >= 0xD800 && code <= 0xDBFF) {
                        unsigned low = 0;
                        if (pos + 6 < json.size() && json[pos + 1] == '\\' &&
                            json[pos + 2] == 'u' && parse_hex4(json, pos + 3, low) &&
                            low >= 0xDC00 && low <= 0xDFFF) {
                            code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                            pos += 6;
                        } else {
                            return {};
                        }
                    } else if (code >= 0xDC00 && code <= 0xDFFF) {
                        return {};
                    }
                    append_utf8(out, code);
                    break;
                }
                default: out.push_back(ch); break;
            }
            escaped = false;
        } else if (ch == '\\') {
            escaped = true;
        } else if (ch == '"') {
            return out;
        } else {
            out.push_back(ch);
        }
    }
    return {};
}

double find_number(const std::string& json, const std::string& key, double fallback) {
    const auto marker = std::string{"\""} + key + "\"";
    auto pos = json.find(marker);
    if (pos == std::string::npos) return fallback;
    pos = json.find(':', pos + marker.size());
    if (pos == std::string::npos) return fallback;
    const char* begin = json.c_str() + pos + 1;
    char* end = nullptr;
    const double value = std::strtod(begin, &end);
    return end == begin ? fallback : value;
}

int find_int(const std::string& json, const std::string& key, int fallback) {
    return static_cast<int>(find_number(json, key, static_cast<double>(fallback)));
}

std::string response(const std::string& id, const AdapterResult& result) {
    std::ostringstream out;
    out << "{\"schema\":\"" << kSchema << "\",\"id\":\"" << json_escape(id)
        << "\",\"ok\":" << (result.ok ? "true" : "false");
    if (result.ok) {
        out << ",\"payload\":" << (result.payload_json.empty() ? "{}" : result.payload_json);
    } else {
        out << ",\"error\":\"" << json_escape(result.error) << "\"";
    }
    out << "}\n";
    return out.str();
}

}  // namespace

std::string json_escape(const std::string& value) {
    std::string out;
    out.reserve(value.size() + 8);
    for (const char ch : value) {
        switch (ch) {
            case '\\': out += "\\\\"; break;
            case '"': out += "\\\""; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default: out.push_back(ch); break;
        }
    }
    return out;
}

std::string ControllerCore::handle_line(const std::string& line) {
    const auto id = find_string(line, "id");
    if (id.empty()) {
        return response("", AdapterResult::failure("request id is missing"));
    }
    if (find_string(line, "schema") != kSchema) {
        return response(id, AdapterResult::failure("request schema mismatch"));
    }
    const auto command = find_string(line, "command");
    AdapterResult result;
    if (command == "hello") {
        result = adapter_.hello();
    } else if (command == "probe_state") {
        result = adapter_.probe_state();
    } else if (command == "story_projection_sample") {
        result = adapter_.story_projection_sample(find_string(line, "request_path"),
                                                  find_string(line, "request_sha256"),
                                                  find_string(line, "output_path"));
    } else if (command == "apply_scenario") {
        result = adapter_.apply_scenario(find_string(line, "scenario_path"),
                                         find_string(line, "scenario_sha256"));
    } else if (command == "record_master") {
        result = adapter_.record_master(find_string(line, "run_dir"),
                                        find_number(line, "duration_s", 0.0));
    } else if (command == "arm_export_lane") {
        result = adapter_.arm_export_lane(find_int(line, "lane", 0),
                                          find_string(line, "camera_tape_path"),
                                          find_string(line, "camera_tape_sha256"),
                                          find_string(line, "replay_path"),
                                          find_string(line, "replay_sha256"),
                                          find_string(line, "run_dir"),
                                          find_string(line, "expected_output_root"),
                                          find_string(line, "render_profile_sha256"));
    } else if (command == "export_lane") {
        result = adapter_.export_lane(find_int(line, "lane", 0),
                                      find_string(line, "camera_tape_path"),
                                      find_string(line, "camera_tape_sha256"),
                                      find_string(line, "replay_path"),
                                      find_string(line, "replay_sha256"),
                                      find_string(line, "run_dir"),
                                      find_string(line, "expected_output_root"),
                                      find_string(line, "render_profile_sha256"));
    } else if (command == "activate_editor") {
        result = adapter_.activate_editor();
    } else if (command == "editor_input") {
        result = adapter_.editor_input(find_int(line, "control", 0));
    } else if (command == "probe_editor") {
        result = adapter_.probe_editor();
    } else if (command == "capture_audio") {
        result = adapter_.capture_audio(find_string(line, "replay_path"),
                                        find_string(line, "replay_sha256"),
                                        find_string(line, "run_dir"),
                                        find_number(line, "duration_s", 0.0));
    } else if (command == "detach") {
        detach_requested_ = true;
        result = AdapterResult::success("{\"detached\":true}");
    } else {
        result = AdapterResult::failure("unknown command: " + command);
    }
    return response(id, result);
}

}  // namespace diogenes::gta
