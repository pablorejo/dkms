import { z } from "zod";

const usernameRegex = /^[a-zA-Z0-9_.-]{3,32}$/;

export const registerSchema = z.object({
  username: z
    .string()
    .min(3, "Username must contain at least 3 characters")
    .max(32, "Username must contain at most 32 characters")
    .regex(usernameRegex, "Username can only contain letters, numbers, _, ., -"),
  email: z.string().email("Invalid email"),
  password: z
    .string()
    .min(8, "Password must contain at least 8 characters")
    .max(128, "Password must contain at most 128 characters")
});

export const loginSchema = z.object({
  identifier: z.string().min(1, "Username or email is required"),
  password: z.string().min(1, "Password is required")
});

export type RegisterInput = z.infer<typeof registerSchema>;
export type LoginInput = z.infer<typeof loginSchema>;
