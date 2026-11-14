#ifndef DATAMINER_FALLOUT_PLAYER_TELEMETRY_V1_H
#define DATAMINER_FALLOUT_PLAYER_TELEMETRY_V1_H

#include <stddef.h>
#include <stdint.h>

#define FPT_SINK_ABI_V1 UINT32_C(1)
#define FPT_SCHEMA_V1 "fallout-player-telemetry/v1"

typedef enum fpt_try_publish_code_v1 {
  FPT_TRY_PUBLISH_ACCEPTED_V1 = 0,
  FPT_TRY_PUBLISH_WOULD_BLOCK_V1 = 1,
  FPT_TRY_PUBLISH_CLOSED_V1 = 2,
  FPT_TRY_PUBLISH_INVALID_ARGUMENT_V1 = 3
} fpt_try_publish_code_v1;

/* Must consume/copy before returning. Must never wait or perform blocking I/O. */
typedef fpt_try_publish_code_v1 (*fpt_try_publish_fn_v1)(
    void *context, const uint8_t *payload, size_t payload_len);

typedef struct fpt_non_blocking_sink_v1 {
  uint32_t abi_version;
  uint32_t reserved;
  void *context;
  fpt_try_publish_fn_v1 try_publish;
} fpt_non_blocking_sink_v1;

#if defined(__cplusplus)
static_assert(sizeof(fpt_non_blocking_sink_v1) == 24,
              "fpt_non_blocking_sink_v1 requires the Windows x64 ABI");
#endif

#endif
