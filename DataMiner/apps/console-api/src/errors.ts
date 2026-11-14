export class ConsoleError extends Error {
  readonly code: string;
  readonly statusCode: number;
  readonly retryable: boolean;
  readonly retryAfterSeconds: number | null;

  constructor(
    code: string,
    message: string,
    statusCode: number,
    options: { retryable?: boolean; retryAfterSeconds?: number } = {},
  ) {
    super(message);
    this.name = "ConsoleError";
    this.code = code;
    this.statusCode = statusCode;
    this.retryable = options.retryable ?? false;
    this.retryAfterSeconds = options.retryAfterSeconds ?? null;
  }
}

export function badRequest(code: string, message: string): ConsoleError {
  return new ConsoleError(code, message, 400);
}

export function unauthorized(code: string, message: string): ConsoleError {
  return new ConsoleError(code, message, 401);
}

export function forbidden(code: string, message: string): ConsoleError {
  return new ConsoleError(code, message, 403);
}

export function notFound(code: string, message: string): ConsoleError {
  return new ConsoleError(code, message, 404);
}

export function conflict(code: string, message: string): ConsoleError {
  return new ConsoleError(code, message, 409);
}

export function tooManyRequests(message: string, retryAfterSeconds: number): ConsoleError {
  return new ConsoleError("rate_limited", message, 429, {
    retryable: true,
    retryAfterSeconds,
  });
}

// Nothing the console writes may make data eligible for training, so a request
// that asks for it is refused rather than downgraded.
export function policyBlocked(message: string): ConsoleError {
  return new ConsoleError("policy_blocked", message, 422);
}

// Authentication failures collapse to one code on purpose. A caller must not be
// able to tell an unknown address from a wrong password, a wrong code, an expired
// challenge, or an exhausted attempt budget.
export function invalidCredentials(): ConsoleError {
  return unauthorized("invalid_credentials", "the credentials could not be verified");
}

// Signup verification is already an explicit operation, so naming that operation
// does not reveal whether an account or challenge exists. The *reason* still stays
// internal: missing, expired, superseded, exhausted and mismatched challenges all
// keep the same public shape.
export function signupVerificationFailed(): ConsoleError {
  return unauthorized(
    "signup_verification_failed",
    "the signup verification code could not be verified",
  );
}
