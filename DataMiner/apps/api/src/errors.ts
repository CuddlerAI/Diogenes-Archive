export class ApiError extends Error {
  readonly code: string;
  readonly statusCode: number;
  readonly retryable: boolean;
  readonly diagnostics: Readonly<Record<string, string | number>> | undefined;

  constructor(
    code: string,
    message: string,
    statusCode: number,
    retryable = false,
    diagnostics?: Readonly<Record<string, string | number>>,
  ) {
    super(message);
    this.name = "ApiError";
    this.code = code;
    this.statusCode = statusCode;
    this.retryable = retryable;
    this.diagnostics = diagnostics;
  }
}

export function conflict(code: string, message: string): ApiError {
  return new ApiError(code, message, 409);
}

export function retryableConflict(
  code: string,
  message: string,
  diagnostics?: Readonly<Record<string, string | number>>,
): ApiError {
  return new ApiError(code, message, 409, true, diagnostics);
}

export function dependencyUnavailable(
  code: string,
  message: string,
  diagnostics?: Readonly<Record<string, string | number>>,
): ApiError {
  return new ApiError(code, message, 503, true, diagnostics);
}

export function notFound(code: string, message: string): ApiError {
  return new ApiError(code, message, 404);
}

export function forbidden(code: string, message: string): ApiError {
  return new ApiError(code, message, 403);
}

export function badRequest(code: string, message: string): ApiError {
  return new ApiError(code, message, 400);
}

export function unauthorized(code: string, message: string): ApiError {
  return new ApiError(code, message, 401);
}

export function policyBlocked(message = "the production endpoint accepts non-trainable RAW artifacts only"): ApiError {
  return new ApiError("policy_blocked", message, 422);
}
