#define DIOGENES_SCRIPT_HOOK_V_ADAPTER 1

#include "controller_core.hpp"
#include "game_adapter.hpp"
#include "pipe_server.hpp"
#include "process_runtime.hpp"

#include <main.h>
#include <windows.h>

#include <atomic>

namespace {

std::atomic<struct AsiProcessRuntime*> g_process_runtime{nullptr};
std::atomic<bool> g_final_frame_callback_registered{false};

using RegisterFinalFrameCallback = void(__cdecl*)(void(__stdcall*)());

struct AsiProcessRuntime {
    AsiProcessRuntime()
        : control(diogenes::gta::make_scripthook_game_adapter()),
          server(LR"(\\.\pipe\diogenes-gta-capture-v1)") {
        server.open();
        g_process_runtime.store(this, std::memory_order_release);
    }

    diogenes::gta::ProcessRuntime control;
    diogenes::gta::PipeServer server;
};

AsiProcessRuntime& process_runtime() {
    // Intentionally process-lifetime. Destruction from DllMain would run under
    // loader lock; Windows reclaims this allocation when GTA exits.
    static auto* runtime = new AsiProcessRuntime();
    return *runtime;
}

void OnPresent(void*) {
    auto* runtime = g_process_runtime.load(std::memory_order_acquire);
    if (runtime != nullptr) runtime->control.adapter().on_present();
}

void __stdcall OnFinalFrameReady() {
    auto* runtime = g_process_runtime.load(std::memory_order_acquire);
    if (runtime != nullptr) runtime->control.adapter().on_present();
}

void RegisterFinalFrameCallbackIfAvailable() {
    if (g_final_frame_callback_registered.load(std::memory_order_acquire)) return;
    const auto addon = GetModuleHandleW(L"IgcsConnector.addon64");
    if (addon == nullptr) return;
    const auto register_callback = reinterpret_cast<RegisterFinalFrameCallback>(
        GetProcAddress(addon, "registerDiogenesFxCaptureObserver"));
    if (register_callback == nullptr) return;
    register_callback(OnFinalFrameReady);
    g_final_frame_callback_registered.store(true, std::memory_order_release);
}

void ScriptMain() {
    auto& runtime = process_runtime();
    const auto generation = runtime.control.begin_generation();
    // ScriptHook may abandon the prior script fiber while a client handle is
    // still associated with this process-lifetime server. Start every launch
    // from a fresh pipe instance without recreating the bound adapter.
    runtime.server.open();
    while (runtime.control.is_active(generation) &&
           !runtime.control.controller().detach_requested()) {
        RegisterFinalFrameCallbackIfAvailable();
        runtime.control.adapter().on_tick();
        runtime.server.poll(runtime.control.controller());
        WAIT(0);
    }
}

}  // namespace

BOOL APIENTRY DllMain(HMODULE module, DWORD reason, LPVOID) {
    if (reason == DLL_PROCESS_ATTACH) {
        DisableThreadLibraryCalls(module);
        presentCallbackRegister(OnPresent);
        scriptRegister(module, ScriptMain);
    } else if (reason == DLL_PROCESS_DETACH) {
        g_process_runtime.store(nullptr, std::memory_order_release);
        presentCallbackUnregister(OnPresent);
        scriptUnregister(module);
    }
    return TRUE;
}
