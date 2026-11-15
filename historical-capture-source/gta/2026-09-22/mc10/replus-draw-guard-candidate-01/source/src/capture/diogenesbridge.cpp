#include "main.h"

#include "capture/diogenesbridge.h"
#include "capture/fxcapture.h"
#include "capture/render.h"
#include "game/game.h"
#include "replay/freecam.h"
#include "replay/marker.h"
#include "replay/quat.h"
#include "ui/exportmenu.h"

#include <bcrypt.h>

#include <array>
#include <atomic>
#include <cmath>
#include <cstring>
#include <cwchar>
#include <limits>
#include <mutex>
#include <string>
#include <vector>

namespace diogenesbridge
{
	namespace
	{
		constexpr const char* kPlanMappingName = "Local\\DiogenesGtaExportPlan";
		constexpr uint32_t kPlanMagic = 0x44474550; // 'DGEP'
		constexpr uint32_t kPlanVersion = 2;
		constexpr uint32_t kMaxFrames = 30u * 60u * 60u; // one hour at 30 fps
		constexpr uint32_t kNoPendingFrame =
			(std::numeric_limits<uint32_t>::max)();

		// replayMode() is a raw CReplayMgr enum, not an Editor-present predicate.
		// Mode 0 is the observed ordinary Story idle state; EDIT is 2 and full-
		// project preview uses another mode. Keep every non-idle/unknown value
		// conservative, including -1 (unresolved), instead of allowing only EDIT.
		constexpr bool editorOrUnknownMode(int mode, bool editMenu, bool rendering)
		{
			return mode != 0 || editMenu || rendering;
		}
		static_assert(!editorOrUnknownMode(0, false, false));
		static_assert(editorOrUnknownMode(1, false, false));
		static_assert(editorOrUnknownMode(2, false, false));
		static_assert(editorOrUnknownMode(3, false, false));
		static_assert(editorOrUnknownMode(-1, false, false));
		static_assert(editorOrUnknownMode(47, false, false));
		static_assert(editorOrUnknownMode(-47, false, false));
		static_assert(editorOrUnknownMode(0, true, false));
		static_assert(editorOrUnknownMode(0, false, true));

		HANDLE s_planMap = nullptr;
		uint8_t* s_planView = nullptr;
		size_t s_planCapacity = 0;
		bool s_ownedExport = false;
		uint32_t s_lastPlannedFrame = 0;
		uint32_t s_pendingPlannedFrame = kNoPendingFrame;
		uint32_t s_finalCameraGeneration = 0;
		uint32_t s_lastMenuAddonHeartbeat = 0;
		uint32_t s_lastUiReadySequence = 0;
		std::atomic<DWORD> s_editorThreadId{0};
		std::atomic<bool> s_menuCursorSuppression{false};
		DiogenesEditorRuntimeEvidence s_lastPlanEvidence{};
		std::mutex s_lastPlanEvidenceMutex;

		bool finite(const double* p, size_t n)
		{
			for (size_t i = 0; i < n; ++i)
				if (!std::isfinite(p[i])) return false;
			return true;
		}

		bool nonzeroHash(const uint8_t* digest)
		{
			uint8_t any = 0;
			for (size_t i = 0; i < 32; ++i) any |= digest[i];
			return any != 0;
		}

		bool sha256File(const std::string& path, std::array<uint8_t, 32>& digest)
		{
			if (path.empty()) return false;
			HANDLE file = CreateFileA(path.c_str(), GENERIC_READ, FILE_SHARE_READ,
			                          nullptr, OPEN_EXISTING, FILE_FLAG_SEQUENTIAL_SCAN,
			                          nullptr);
			if (file == INVALID_HANDLE_VALUE) return false;
			BCRYPT_ALG_HANDLE algorithm = nullptr;
			BCRYPT_HASH_HANDLE hash = nullptr;
			DWORD objectBytes = 0;
			DWORD digestBytes = 0;
			DWORD resultBytes = 0;
			std::vector<UCHAR> object;
			std::vector<UCHAR> chunk(64 * 1024);
			bool ok = false;

			if (!BCRYPT_SUCCESS(BCryptOpenAlgorithmProvider(
			        &algorithm, BCRYPT_SHA256_ALGORITHM, nullptr, 0))) goto done;
			if (!BCRYPT_SUCCESS(BCryptGetProperty(
			        algorithm, BCRYPT_OBJECT_LENGTH,
			        reinterpret_cast<PUCHAR>(&objectBytes), sizeof(objectBytes),
			        &resultBytes, 0)) || resultBytes != sizeof(objectBytes) ||
			    objectBytes == 0) goto done;
			if (!BCRYPT_SUCCESS(BCryptGetProperty(
			        algorithm, BCRYPT_HASH_LENGTH,
			        reinterpret_cast<PUCHAR>(&digestBytes), sizeof(digestBytes),
			        &resultBytes, 0)) || resultBytes != sizeof(digestBytes) ||
			    digestBytes != digest.size()) goto done;

			object.resize(objectBytes);
			if (!BCRYPT_SUCCESS(BCryptCreateHash(
			        algorithm, &hash, object.data(), objectBytes, nullptr, 0, 0))) goto done;
			for (;;)
			{
				DWORD count = 0;
				if (!ReadFile(file, chunk.data(), static_cast<DWORD>(chunk.size()),
				              &count, nullptr)) goto done;
				if (count == 0) break;
				if (!BCRYPT_SUCCESS(BCryptHashData(hash, chunk.data(), count, 0))) goto done;
			}
			if (!BCRYPT_SUCCESS(BCryptFinishHash(
			        hash, digest.data(), static_cast<ULONG>(digest.size()), 0))) goto done;
			ok = true;

		done:
			if (hash) BCryptDestroyHash(hash);
			if (algorithm) BCryptCloseAlgorithmProvider(algorithm, 0);
			CloseHandle(file);
			return ok;
		}

		bool fixedString(const char* value, size_t capacity, std::string& out)
		{
			const void* terminator = memchr(value, '\0', capacity);
			if (!terminator) return false;
			const size_t length = static_cast<const char*>(terminator) - value;
			if (length == 0) return false;
			for (size_t i = length + 1; i < capacity; ++i)
				if (value[i] != '\0') return false;
			out.assign(value, length);
			return true;
		}

		bool normalizeOutputRoot(const std::string& utf8, std::string& normalized,
		                         std::wstring& normalizedWide)
		{
			if (utf8.empty()) return false;
			const int wideCount = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS,
			                                          utf8.c_str(), -1, nullptr, 0);
			if (wideCount <= 1) return false;
			std::vector<wchar_t> wide(static_cast<size_t>(wideCount));
			if (!MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, utf8.c_str(), -1,
			                         wide.data(), wideCount)) return false;
			const DWORD needed = GetFullPathNameW(wide.data(), 0, nullptr, nullptr);
			if (needed == 0) return false;
			std::vector<wchar_t> full(static_cast<size_t>(needed) + 1u);
			if (GetFullPathNameW(wide.data(), static_cast<DWORD>(full.size()),
			                     full.data(), nullptr) == 0) return false;
			normalizedWide.assign(full.data());
			if (normalizedWide.size() < 3 || normalizedWide[1] != L':' ||
			    (normalizedWide[2] != L'\\' && normalizedWide[2] != L'/')) return false;
			for (auto& ch : normalizedWide) if (ch == L'/') ch = L'\\';
			while (normalizedWide.size() > 3 && normalizedWide.back() == L'\\')
				normalizedWide.pop_back();
			const int byteCount = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS,
			                                          normalizedWide.c_str(), -1,
			                                          nullptr, 0, nullptr, nullptr);
			if (byteCount <= 1 || static_cast<size_t>(byteCount) > 512u) return false;
			std::vector<char> bytes(static_cast<size_t>(byteCount));
			if (!WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS,
			                         normalizedWide.c_str(), -1, bytes.data(), byteCount,
			                         nullptr, nullptr)) return false;
			normalized.assign(bytes.data());
			return true;
		}

		void openPlan()
		{
			if (s_planView) return;
			s_planMap = OpenFileMappingA(FILE_MAP_ALL_ACCESS, FALSE, kPlanMappingName);
			if (!s_planMap) return;
			s_planView = static_cast<uint8_t*>(
				MapViewOfFile(s_planMap, FILE_MAP_ALL_ACCESS, 0, 0, 0));
			if (!s_planView)
			{
				CloseHandle(s_planMap);
				s_planMap = nullptr;
				return;
			}
			MEMORY_BASIC_INFORMATION info{};
			if (VirtualQuery(s_planView, &info, sizeof(info)) == sizeof(info))
				s_planCapacity = info.RegionSize;
		}

		bool snapshotHeader(DiogenesExportPlanHeader& out)
		{
			if (!s_planView || s_planCapacity < sizeof(out)) return false;
			auto* live = reinterpret_cast<volatile DiogenesExportPlanHeader*>(s_planView);
			for (int attempt = 0; attempt < 4; ++attempt)
			{
				const uint32_t before = live->sequence;
				if (before & 1u) continue;
				std::atomic_thread_fence(std::memory_order_acquire);
				memcpy(&out, s_planView, sizeof(out));
				std::atomic_thread_fence(std::memory_order_acquire);
				const uint32_t after = live->sequence;
				if (before == after && !(after & 1u)) return true;
			}
			return false;
		}

		bool validHeader(const DiogenesExportPlanHeader& h)
		{
			if (h.magic != kPlanMagic || h.version != kPlanVersion ||
			    h.headerSize != sizeof(DiogenesExportPlanHeader) ||
			    h.sampleStride != sizeof(DiogenesExportPlanSample) ||
			    h.publisherPid != GetCurrentProcessId() || h.lane < 1 || h.lane > 4 ||
			    h.frameCount == 0 || h.frameCount > kMaxFrames ||
			    h.fpsNum != 30 || h.fpsDen != 1 || h.width == 0 || h.height == 0 ||
			    h.state > PLAN_FAILED || !nonzeroHash(h.tapeSha256) ||
			    !nonzeroHash(h.renderProfileSha256)) return false;
			std::string outputRoot;
			if (!fixedString(h.expectedOutputRootUtf8,
			                 sizeof(h.expectedOutputRootUtf8), outputRoot)) return false;

			const uint64_t expected = sizeof(DiogenesExportPlanHeader) +
				static_cast<uint64_t>(h.frameCount) * sizeof(DiogenesExportPlanSample);
			if (expected != h.mappingSize || expected > s_planCapacity) return false;
			for (uint8_t byte : h.reserved) if (byte != 0) return false;
			return true;
		}

		bool configValid(const DiogenesExportPlanHeader& h,
		                 DiogenesEditorRuntimeEvidence& evidence)
		{
			const Config& c = Config::get();
			std::array<uint8_t, 32> actualProfileDigest{};
			std::string expectedRoot;
			std::string configuredRoot;
			std::wstring expectedWide;
			std::wstring configuredWide;
			if (!sha256File(c.renderIniPath, actualProfileDigest)) return false;
			memcpy(evidence.renderProfileSha256, actualProfileDigest.data(),
			       sizeof(evidence.renderProfileSha256));
			if (memcmp(actualProfileDigest.data(), h.renderProfileSha256,
			           actualProfileDigest.size()) != 0 ||
			    !fixedString(h.expectedOutputRootUtf8,
			                 sizeof(h.expectedOutputRootUtf8), expectedRoot) ||
			    !normalizeOutputRoot(expectedRoot, expectedRoot, expectedWide) ||
			    !normalizeOutputRoot(c.renderOutputFolder, configuredRoot, configuredWide) ||
			    _wcsicmp(expectedWide.c_str(), configuredWide.c_str()) != 0) return false;
			const bool valid =
			       c.enableRenderer && c.renderMode == Config::RenderMode::Frames &&
			       c.renderCaptureMode == 0 && !c.renderDof &&
			       !c.renderJpeg && std::fabs(c.renderFps - 30.0f) < 0.0001f &&
			       c.renderSamples == 1 && c.renderSettleFrames == 3 &&
			       c.renderSettleSubFrames == 1 && c.renderKeepFrames &&
			       c.renderAudio && c.renderMarkerSpeed && c.renderHideHud &&
			       std::fabs(c.renderHighlight - 0.9f) < 0.0001f &&
				       c.exportCloseWhenDone;
			if (!valid) return false;
			memset(evidence.normalizedOutputRoot, 0,
			       sizeof(evidence.normalizedOutputRoot));
			memcpy(evidence.normalizedOutputRoot, configuredRoot.data(),
			       configuredRoot.size());
			return true;
		}

		bool renderCoverageValid(const DiogenesExportPlanHeader& h, int renderFrames,
		                         bool& known)
		{
			known = false;
			const int clips = game::clipCount();
			if (clips <= 0) return false;
			known = true;
			if (clips != 1 || renderFrames < static_cast<int>(h.frameCount)) return false;
			float lo = 0.0f;
			float hi = 0.0f;
			if (!game::clipRange(lo, hi) || !std::isfinite(lo) || !std::isfinite(hi) ||
			    hi <= lo) return false;
			const double frameMs = 1000.0 * static_cast<double>(h.fpsDen) /
			                       static_cast<double>(h.fpsNum);
			const double durationMs = static_cast<double>(hi) - static_cast<double>(lo);
			const double lastPlannedMs =
				static_cast<double>(h.frameCount - 1u) * frameMs;
			// Keep the renderer-bound check on exactly the same duration source and
			// float arithmetic as render::begin(). clipRange() is the non-dilated
			// replay span used for coverage; the renderer counts output frames from
			// totalDilatedMs() when RenderMarkerSpeed=1. Recomputing the bound from
			// the former can reject a valid project even at 100% marker speed because
			// the engine exposes the two endpoints with slightly different rounding.
			const float renderDurationMs = game::totalDilatedMs();
			const float renderFrameMs = 1000.0f * static_cast<float>(h.fpsDen) /
			                            static_cast<float>(h.fpsNum);
			if (!std::isfinite(renderDurationMs) || renderDurationMs <= 1.0f ||
			    !std::isfinite(renderFrameMs) || renderFrameMs <= 0.0f) return false;
			const int expectedBound =
				static_cast<int>(renderDurationMs / renderFrameMs + 0.5f) + 2;
			const bool valid = durationMs + 0.5 >= lastPlannedMs &&
			                   renderFrames == expectedBound;
			if (!valid)
			{
				logger::write("info",
					"diogenes: render coverage mismatch clip=%.3f..%.3f span=%.3f last=%.3f output=%.3f frameMs=%.6f renderFrames=%d expected=%d",
					(double)lo, (double)hi, durationMs, lastPlannedMs,
					(double)renderDurationMs, (double)renderFrameMs,
					renderFrames, expectedBound);
			}
			return valid;
		}

		bool clipMatches(uint64_t wanted)
		{
			std::vector<uint64_t> ids;
			const int at = game::clipIndex();
			return wanted != 0 && game::clipIdentities(ids) && at >= 0 &&
			       at < static_cast<int>(ids.size()) && ids[static_cast<size_t>(at)] == wanted;
		}

		bool snapshotSample(const DiogenesExportPlanHeader& h, uint32_t frame,
		                    DiogenesExportPlanSample& out)
		{
			if (frame >= h.frameCount) return false;
			auto* live = reinterpret_cast<volatile DiogenesExportPlanHeader*>(s_planView);
			const uint32_t before = live->sequence;
			if (before != h.sequence || (before & 1u)) return false;
			const size_t offset = h.headerSize + static_cast<size_t>(frame) * h.sampleStride;
			memcpy(&out, s_planView + offset, sizeof(out));
			std::atomic_thread_fence(std::memory_order_acquire);
			if (live->sequence != before) return false;

			const double q2 = out.quaternion[0] * out.quaternion[0] +
			                  out.quaternion[1] * out.quaternion[1] +
			                  out.quaternion[2] * out.quaternion[2] +
			                  out.quaternion[3] * out.quaternion[3];
			const uint64_t expectedT = (static_cast<uint64_t>(frame) * 1000000000ull + 15ull) / 30ull;
			if (out.frame != frame || out.reserved != 0 || out.source_t_ns != expectedT ||
			    !finite(out.position, 3) || !finite(out.quaternion, 4) ||
			    !finite(out.euler_deg, 3) || !finite(out.intrinsics, 4) ||
			    std::fabs(q2 - 1.0) > 0.002 || out.intrinsics[0] <= 0.0 ||
			    out.intrinsics[1] <= 0.0 ||
			    std::fabs(out.intrinsics[0] - out.intrinsics[1]) > 0.0001)
				return false;

			for (double value : out.position)
				if (std::fabs(value) > static_cast<double>((std::numeric_limits<float>::max)()))
					return false;

			constexpr double kPi = 3.14159265358979323846;
			rfreecam::Mat34 eulerBasis{};
			lookat::basisFromHeadingPitchRoll(
				static_cast<float>(out.euler_deg[2] * kPi / 180.0),
				static_cast<float>(out.euler_deg[0] * kPi / 180.0),
				static_cast<float>(out.euler_deg[1] * kPi / 180.0), eulerBasis);
			const rquat::Quat eq = rquat::fromBasis(eulerBasis.a, eulerBasis.b, eulerBasis.c);
			const double agreement = std::fabs(eq.x * out.quaternion[0] +
			                                   eq.y * out.quaternion[1] +
			                                   eq.z * out.quaternion[2] +
			                                   eq.w * out.quaternion[3]);
			return agreement >= 0.999999;
		}

		bool validateAllSamples(const DiogenesExportPlanHeader& h)
		{
			DiogenesExportPlanSample sample{};
			for (uint32_t frame = 0; frame < h.frameCount; ++frame)
				if (!snapshotSample(h, frame, sample)) return false;
			return true;
		}

		bool transitionState(uint32_t expected, uint32_t state)
		{
			if (!s_planView) return false;
			auto* h = reinterpret_cast<volatile DiogenesExportPlanHeader*>(s_planView);
			for (int attempt = 0; attempt < 16; ++attempt)
			{
				const LONG sequence = InterlockedCompareExchange(
					reinterpret_cast<volatile LONG*>(&h->sequence), 0, 0);
				if (sequence & 1) continue;
				if (InterlockedCompareExchange(
						reinterpret_cast<volatile LONG*>(&h->sequence), sequence + 1,
						sequence) != sequence) continue;
				std::atomic_thread_fence(std::memory_order_acquire);
				const bool changed = h->state == expected;
				if (changed) h->state = state;
				std::atomic_thread_fence(std::memory_order_release);
				InterlockedExchange(reinterpret_cast<volatile LONG*>(&h->sequence),
				                    sequence + 2);
				return changed;
			}
			return false;
		}

		bool readFinalPose(void* director, DiogenesEditorRuntimeEvidence& e)
		{
			if (!director) return false;
			const auto* b = static_cast<const uint8_t*>(director);
			const spline::Vec3 a(reinterpret_cast<const float*>(b + rdirector::OFF_FrameMatrixA));
			const spline::Vec3 f(reinterpret_cast<const float*>(b + rdirector::OFF_FrameMatrixB));
			const spline::Vec3 u(reinterpret_cast<const float*>(b + rdirector::OFF_FrameMatrixC));
			const spline::Vec3 p(reinterpret_cast<const float*>(b + rdirector::OFF_FramePosition));
			const float fov = *reinterpret_cast<const float*>(b + rdirector::OFF_FrameFov);
			const rquat::Quat q = rquat::fromBasis(a, f, u);
			const double pose[] = {p.x, p.y, p.z, q.x, q.y, q.z, q.w, fov};
			if (!finite(pose, sizeof(pose) / sizeof(pose[0])) || fov < 1.0f || fov > 130.0f)
				return false;
			e.finalPosition[0] = p.x; e.finalPosition[1] = p.y; e.finalPosition[2] = p.z;
			e.finalQuaternion[0] = q.x; e.finalQuaternion[1] = q.y;
			e.finalQuaternion[2] = q.z; e.finalQuaternion[3] = q.w;
			e.finalFovDeg = fov;
			return true;
		}

		bool applySample(void* director, const DiogenesExportPlanHeader& h,
		                 const DiogenesExportPlanSample& s,
		                 DiogenesEditorRuntimeEvidence& evidence,
		                 bool advanceGeneration)
		{
			if (!director) return false;
			auto* b = static_cast<uint8_t*>(director);
			float* a = reinterpret_cast<float*>(b + rdirector::OFF_FrameMatrixA);
			float* f = reinterpret_cast<float*>(b + rdirector::OFF_FrameMatrixB);
			float* u = reinterpret_cast<float*>(b + rdirector::OFF_FrameMatrixC);
			float* p = reinterpret_cast<float*>(b + rdirector::OFF_FramePosition);
			const rquat::Quat wanted(static_cast<float>(s.quaternion[0]),
			                          static_cast<float>(s.quaternion[1]),
			                          static_cast<float>(s.quaternion[2]),
			                          static_cast<float>(s.quaternion[3]));
			rquat::toMatrixRows(wanted, a, f, u);
			p[0] = static_cast<float>(s.position[0]);
			p[1] = static_cast<float>(s.position[1]);
			p[2] = static_cast<float>(s.position[2]);
			constexpr double kPi = 3.14159265358979323846;
			const double fov = 2.0 * std::atan(static_cast<double>(h.height) /
			                                   (2.0 * s.intrinsics[1])) * 180.0 / kPi;
			if (!std::isfinite(fov) || fov < 1.0 || fov > 130.0) return false;
			*reinterpret_cast<float*>(b + rdirector::OFF_FrameFov) = static_cast<float>(fov);

			if (!readFinalPose(director, evidence)) return false;
			const double dx = evidence.finalPosition[0] - s.position[0];
			const double dy = evidence.finalPosition[1] - s.position[1];
			const double dz = evidence.finalPosition[2] - s.position[2];
			const double dot = std::fabs(evidence.finalQuaternion[0] * s.quaternion[0] +
			                             evidence.finalQuaternion[1] * s.quaternion[1] +
			                             evidence.finalQuaternion[2] * s.quaternion[2] +
			                             evidence.finalQuaternion[3] * s.quaternion[3]);
			// The director stores its basis as float. A float matrix round-trip can
			// lose a few 1e-8 in quaternion dot agreement even when the exact sample
			// was written (the four POC lanes bottom out at 0.999999971). Keep this
			// fence well inside 0.052 degrees without demanding double precision from
			// a single-precision engine structure.
			const bool matched = std::sqrt(dx * dx + dy * dy + dz * dz) <= 0.0005 &&
			                     dot >= 0.9999999 &&
			                     std::fabs(evidence.finalFovDeg - fov) <= 0.0002;
			if (matched && advanceGeneration)
				evidence.finalCameraGeneration = ++s_finalCameraGeneration;
			return matched;
		}

		void storeLastPlanEvidence(const DiogenesEditorRuntimeEvidence& evidence)
		{
			std::lock_guard<std::mutex> lock(s_lastPlanEvidenceMutex);
			s_lastPlanEvidence = evidence;
		}

		DiogenesEditorRuntimeEvidence loadLastPlanEvidence()
		{
			std::lock_guard<std::mutex> lock(s_lastPlanEvidenceMutex);
			return s_lastPlanEvidence;
		}
	}

	bool snapshotCaptureEvidence(uint32_t sourceFrame,
	                             DiogenesEditorRuntimeEvidence& evidence,
	                             bool& required)
	{
		required = false;
		openPlan();
		DiogenesExportPlanHeader h{};
		if (!snapshotHeader(h) || !validHeader(h))
		{
			// Once this process has joined a plan mapping, an unstable/corrupt
			// snapshot is never an ordinary unowned export. Publishing a generic
			// zero-generation request here creates an impossible request that the
			// native observer can neither accept nor drain.
			required = s_planView != nullptr;
			if (required)
				logger::write("info",
					"diogenes: capture evidence snapshot unavailable for frame %u - request withheld",
					sourceFrame);
			return !required;
		}
		if (h.state == PLAN_FAILED)
		{
			required = true;
			return false;
		}
		required = h.state == PLAN_TRIGGERED || h.state == PLAN_EXPORTING;
		if (!required) return true;

		std::lock_guard<std::mutex> lock(s_lastPlanEvidenceMutex);
		if (s_lastPlanEvidence.plannedFrame != sourceFrame + 1u ||
		    s_lastPlanEvidence.finalCameraGeneration == 0)
			return false;
		evidence = s_lastPlanEvidence;
		return true;
	}

	bool ownedCaptureEvidenceReady(uint32_t sourceFrame)
	{
		// Ownership is game-thread state: never inspect it from Present or
		// from an unobserved frontend thread. No window-thread inference.
		if (s_editorThreadId.load(std::memory_order_acquire) != GetCurrentThreadId())
			return false;
		DiogenesExportPlanHeader h{};
		if (!s_ownedExport || !snapshotHeader(h) || !validHeader(h) ||
		    h.state != PLAN_EXPORTING || sourceFrame >= h.frameCount)
			return false;
		DiogenesEditorRuntimeEvidence evidence{};
		bool required = false;
		return snapshotCaptureEvidence(sourceFrame, evidence, required) && required &&
		       evidence.renderActive && evidence.exportConfigValid &&
		       evidence.plannedFrame == sourceFrame + 1u && evidence.finalCameraGeneration != 0;
	}

	void tickFinalCamera(void* director)
	{
		const DWORD currentThread = GetCurrentThreadId();
		DWORD expectedThread = 0;
		if (!s_editorThreadId.compare_exchange_strong(
		        expectedThread, currentThread, std::memory_order_acq_rel) &&
		    expectedThread != currentThread) return;

		DiogenesEditorRuntimeEvidence evidence{};
		evidence.editMenuActive = exportmenu::exportSettingsActive();
		evidence.renderActive = render::active();
		evidence.editorActive = editorOrUnknownMode(
			game::replayMode(), evidence.editMenuActive, evidence.renderActive);
		readFinalPose(director, evidence);

		openPlan();
		DiogenesExportPlanHeader h{};
		const bool headerOk = snapshotHeader(h) && validHeader(h);
		evidence.exportConfigValid = headerOk && configValid(h, evidence);
		evidence.startButtonArmed = evidence.editMenuActive &&
		                              exportmenu::startExportFocused() &&
		                              evidence.exportConfigValid;

		const bool framePass = render::framePassActive();
		if (headerOk && s_ownedExport && h.state == PLAN_FAILED)
		{
			// Native rejected a request-bound pose/pixel fact. Stop producing new
			// frames. Any already-published request remains stable until the native
			// failure drainer observes pixel-ready, releases it, and sees add-on ack.
			render::cancel();
			s_ownedExport = false;
		}
		else if (headerOk && s_ownedExport && h.state == PLAN_EXPORTING && !framePass)
		{
			const bool completed = render::completionMatches(static_cast<int>(h.frameCount));
			if (s_lastPlannedFrame != h.frameCount || !evidence.exportConfigValid || !completed)
				logger::write("info",
					"diogenes: frame pass ended before a sealed plan receipt (planned=%u expected=%u config=%d receipt=%d)",
					s_lastPlannedFrame, h.frameCount, (int)evidence.exportConfigValid, (int)completed);
			transitionState(PLAN_EXPORTING,
			                s_lastPlannedFrame == h.frameCount && evidence.exportConfigValid && completed
			                    ? PLAN_DONE : PLAN_FAILED);
			s_ownedExport = false;
		}
		else if (headerOk && framePass &&
		         (h.state == PLAN_TRIGGERED || (s_ownedExport && h.state == PLAN_EXPORTING)))
		{
			const int frame = render::frameDone();
			const int renderFrames = render::frameCount();
			if (h.state == PLAN_TRIGGERED)
			{
				s_lastPlannedFrame = 0;
				storeLastPlanEvidence({});
			}
			bool renderCoverageKnown = false;
			const bool renderCoverageReady =
				renderCoverageValid(h, renderFrames, renderCoverageKnown);
			const bool terminalBoundSlack =
				h.state == PLAN_EXPORTING && renderCoverageReady &&
				renderFrames > static_cast<int>(h.frameCount) && frame >= 0 &&
				static_cast<uint32_t>(frame) == h.frameCount &&
				s_lastPlannedFrame == h.frameCount;
			if (terminalBoundSlack)
			{
				// All sealed requests have already crossed the final-camera and pixel
				// ACK fences. End before RE+ can consume its non-semantic bound slack;
				// no frame is duplicated, held, or assigned an invented replay clock.
				if (!evidence.exportConfigValid ||
				    !render::completeAfterAckedFrames(static_cast<int>(h.frameCount)) ||
				    !transitionState(PLAN_EXPORTING, PLAN_DONE))
				{
					transitionState(PLAN_EXPORTING, PLAN_FAILED);
					render::cancel();
					s_ownedExport = false;
					fxcapture::publishEditorIdentity(fxcapture::requestId(), evidence);
					return;
				}
				logger::write("info",
					"diogenes: renderer completed after %u acknowledged frames; bound slack elided (bound=%d)",
					h.frameCount, renderFrames);
				s_ownedExport = false;
				fxcapture::publishEditorIdentity(fxcapture::requestId(), evidence);
				return;
			}
			const bool alreadyApplied = h.state == PLAN_EXPORTING && frame >= 0 &&
				                         s_lastPlannedFrame == static_cast<uint32_t>(frame) + 1;
			if (alreadyApplied)
			{
				if (!evidence.exportConfigValid)
				{
					transitionState(PLAN_EXPORTING, PLAN_FAILED);
					render::cancel();
					s_ownedExport = false;
					fxcapture::publishEditorIdentity(fxcapture::requestId(), evidence);
					return;
				}
				// The stock director rebuilds its output on every UpdateSmoothing,
				// including the settle/request wait. Restore the same planned sample as
				// absolute last writer on every tick, while preserving the single
				// generation allocated when this source frame was first accepted.
				evidence = loadLastPlanEvidence();
				DiogenesExportPlanSample repeat{};
				if (!snapshotSample(h, static_cast<uint32_t>(frame), repeat) ||
				    !applySample(director, h, repeat, evidence, false) ||
				    evidence.finalCameraGeneration == 0)
				{
					logger::write("info",
						"diogenes: accepted camera could not be restored at frame %d",
						frame);
					transitionState(PLAN_EXPORTING, PLAN_FAILED);
					render::cancel();
					s_ownedExport = false;
					fxcapture::publishEditorIdentity(fxcapture::requestId(), evidence);
					return;
				}
				storeLastPlanEvidence(evidence);
				fxcapture::publishEditorIdentity(fxcapture::requestId(), evidence);
				return;
			}
			const bool firstFrame = h.state != PLAN_TRIGGERED || frame == 0;
			const bool wholePlanValid = h.state != PLAN_TRIGGERED || validateAllSamples(h);
			const bool structuralBase =
				firstFrame && wholePlanValid && evidence.exportConfigValid && frame >= 0 &&
				static_cast<uint32_t>(frame) < h.frameCount &&
				s_lastPlannedFrame == static_cast<uint32_t>(frame);
			if (structuralBase && !renderCoverageKnown)
			{
				// The playback controller can briefly hide its clip table while
				// rewinding from the audio pass. Wait for a coherent range; the native
				// trigger/watchdog still fails closed if it never becomes readable.
				fxcapture::publishEditorIdentity(fxcapture::requestId(), evidence);
				return;
			}
			const bool structuralReady = structuralBase && renderCoverageReady;
			const bool clipReady = structuralReady && clipMatches(h.masterClipIdentity);
			if (clipReady && s_pendingPlannedFrame != static_cast<uint32_t>(frame))
			{
				// render::pump() advances Ack -> seekAndSettle() earlier in the same
				// UpdateSmoothing hook. Applying the next camera immediately here races
				// the editor's new playhead/clip state. Observe the new frame once, then
				// wait for one complete origUpdateSmoothing() before validating and
				// writing it. The renderer is still in its three-tick settle window.
				s_pendingPlannedFrame = static_cast<uint32_t>(frame);
				fxcapture::publishEditorIdentity(fxcapture::requestId(), evidence);
				return;
			}
			if (structuralReady && !clipReady)
			{
				// A seek can temporarily expose no current clip. Treat that as a wait,
				// not a rejected plan. If it does not settle before capture, the request
				// evidence fence still aborts the renderer fail-closed.
				s_pendingPlannedFrame = kNoPendingFrame;
				fxcapture::publishEditorIdentity(fxcapture::requestId(), evidence);
				return;
			}
			DiogenesExportPlanSample sample{};
			const bool sampleValid = structuralReady && clipReady && frame >= 0 &&
				snapshotSample(h, static_cast<uint32_t>(frame), sample);
			const bool sampleApplied = sampleValid &&
				applySample(director, h, sample, evidence, true);
			if (sampleApplied)
			{
				if (h.state == PLAN_TRIGGERED)
				{
					if (!transitionState(PLAN_TRIGGERED, PLAN_EXPORTING))
					{
						render::cancel();
						return;
					}
					s_ownedExport = true;
				}
				evidence.plannedFrame = static_cast<uint32_t>(frame) + 1;
				s_lastPlannedFrame = evidence.plannedFrame;
				storeLastPlanEvidence(evidence);
			}
			else
			{
				logger::write("info",
					"diogenes: camera plan rejected at frame %d (state=%u structural=%d sample=%d applied=%d last=%u count=%u config=%d clip=%d renderFrames=%d pending=%u)",
					frame, h.state, (int)structuralReady, (int)sampleValid, (int)sampleApplied,
					s_lastPlannedFrame, h.frameCount, (int)evidence.exportConfigValid,
					(int)clipMatches(h.masterClipIdentity), render::frameCount(),
					s_pendingPlannedFrame);
				transitionState(h.state, PLAN_FAILED);
				if (s_ownedExport) render::cancel();
				s_ownedExport = false;
			}
		}
		else if (s_ownedExport && (!headerOk || !evidence.exportConfigValid))
		{
			// Only write through a still-recognisable mapping. A corrupt magic or
			// truncated view is untrusted memory and remains untouched.
			if (s_planView && h.magic == kPlanMagic && h.version == kPlanVersion)
				transitionState(h.state, PLAN_FAILED);
			render::cancel();
			s_ownedExport = false;
		}

		fxcapture::publishEditorIdentity(fxcapture::requestId(), evidence);
	}

	bool menuCursorSuppressionArmed()
	{
		if (!s_menuCursorSuppression.load(std::memory_order_acquire)) return false;
		DWORD foregroundPid = 0;
		GetWindowThreadProcessId(GetForegroundWindow(), &foregroundPid);
		return foregroundPid == GetCurrentProcessId();
	}

	void tickMenuIdentity()
	{
		// MousePointerUpdate is the editor UI's per-frame heartbeat. Collapse any
		// duplicate calls to one publication per actual Present generation, so the
		// native observer can prove both producers are live without manufacturing
		// extra identity generations inside one frame.
		const DWORD editorThread = s_editorThreadId.load(std::memory_order_acquire);
		if (editorThread == 0 || editorThread != GetCurrentThreadId()) return;
		if (!exportmenu::exportSettingsActive())
		{
			s_menuCursorSuppression.store(false, std::memory_order_release);
			s_lastMenuAddonHeartbeat = 0;
			return;
		}
		const uint32_t addonHeartbeat = fxcapture::heartbeat();
		if (addonHeartbeat == 0)
		{
			s_menuCursorSuppression.store(false, std::memory_order_release);
			return;
		}
		if (addonHeartbeat == s_lastMenuAddonHeartbeat) return;

		openPlan();
		DiogenesExportPlanHeader h{};
		if (!snapshotHeader(h) || !validHeader(h) ||
		    (h.state != PLAN_ARMED && h.state != PLAN_TRIGGERED))
		{
			s_menuCursorSuppression.store(false, std::memory_order_release);
			return;
		}

		DiogenesEditorRuntimeEvidence evidence{};
		evidence.editMenuActive = true;
		evidence.renderActive = render::active();
		evidence.editorActive = editorOrUnknownMode(
			game::replayMode(), evidence.editMenuActive, evidence.renderActive);
		evidence.exportConfigValid = configValid(h, evidence);
		if (h.state == PLAN_ARMED && !evidence.renderActive &&
		    evidence.exportConfigValid)
		{
			// Selection alone is inert; the native observer must still prove two
			// fresh identities and publish TRIGGERED. The operator/Computer Use
			// then completes the stock Export/name/confirmation flow. Do not call
			// Playback::Open from a pointer-update callback: that skips frontend
			// teardown and was observed to retain a cursor in every raw frame.
			exportmenu::focusStartExportForArmedPlan();
		}
		evidence.startButtonArmed = !evidence.renderActive &&
		                              exportmenu::startExportFocused() &&
		                              evidence.exportConfigValid;
		DWORD foregroundPid = 0;
		GetWindowThreadProcessId(GetForegroundWindow(), &foregroundPid);
		// Keep normal pointer handling alive through the stock confirmation UI.
		// Frame-pass suppression is armed separately before requesting any PNG.
		s_menuCursorSuppression.store(false, std::memory_order_release);
		fxcapture::publishEditorIdentity(fxcapture::requestId(), evidence);
		s_lastMenuAddonHeartbeat = addonHeartbeat;

		// TRIGGERED authorizes capture; it does not replace the game's UI state
		// machine. The existing bounded plan wait remains active while the stock
		// Export confirmation closes the menu and enters Playback normally.
		// No synthetic input, direct playback call or camera write occurs here.
		if (h.state != PLAN_TRIGGERED || !evidence.startButtonArmed ||
		    h.publisherPid != GetCurrentProcessId() ||
		    h.sequence == s_lastUiReadySequence) return;
		if (foregroundPid != GetCurrentProcessId()) return;
		s_lastUiReadySequence = h.sequence;
		logger::write("info",
		              "diogenes: authorized plan lane %u ready for stock Export confirmation",
		              h.lane);
	}
}
