import { z } from "zod";

export const emailSchema = z
  .string()
  .trim()
  .toLowerCase()
  .email()
  .max(254);

export const challengeIdSchema = z.string().uuid();

// The submitted code is normalised but never trimmed into a different length:
// a six digit code is the only accepted shape.
export const otpCodeSchema = z
  .string()
  .trim()
  .regex(/^[0-9]{6}$/, "a six digit verification code is required");

export const opaqueTokenSchema = z
  .string()
  .trim()
  .regex(/^[A-Za-z0-9_-]{43}$/, "an invite or reset token is required");

export const loginSchema = z.object({
  email: emailSchema,
  password: z.string().min(1).max(1024),
});

export const otpVerifySchema = z.object({
  challengeId: challengeIdSchema,
  code: otpCodeSchema,
});

export const otpResendSchema = z.object({
  challengeId: challengeIdSchema,
});

export const passwordResetRequestSchema = z.object({
  email: emailSchema,
});

/**
 * Password is optional because `cf_access` has no console password to re-enter:
 * a JIT-provisioned operator holds an unusable random hash, so under that mode
 * step-up re-verifies the Access assertion instead. The route picks the factor
 * from the configured login mode, not from which field the caller sent — a body
 * that carries a password under `cf_access` is simply ignored.
 */
export const sudoSchema = z.object({
  password: z.string().min(1).max(1024).optional(),
});

export const createOperatorSchema = z.object({
  email: emailSchema,
  role: z.enum(["viewer", "operator", "admin"]),
});

export const updateOperatorSchema = z
  .object({
    role: z.enum(["viewer", "operator", "admin"]).optional(),
    status: z.enum(["active", "suspended"]).optional(),
  })
  .refine((value) => value.role !== undefined || value.status !== undefined, {
    message: "at least one of role or status must be provided",
  });

export const operatorIdParamsSchema = z.object({
  operatorId: z.string().uuid(),
});

export function passwordSchema(minLength: number) {
  return z.string().min(minLength).max(1024);
}

export function acceptInviteSchema(minLength: number) {
  return z.object({
    challengeId: challengeIdSchema,
    token: opaqueTokenSchema,
    password: passwordSchema(minLength),
  });
}

export function passwordResetConfirmSchema(minLength: number) {
  return z.object({
    challengeId: challengeIdSchema,
    token: opaqueTokenSchema,
    password: passwordSchema(minLength),
  });
}

export type LoginInput = z.infer<typeof loginSchema>;
export type OtpVerifyInput = z.infer<typeof otpVerifySchema>;
export type CreateOperatorRequest = z.infer<typeof createOperatorSchema>;
export type UpdateOperatorRequest = z.infer<typeof updateOperatorSchema>;
