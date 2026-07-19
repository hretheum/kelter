import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/* Global focus ring (design/05 §3.2): 2px accent ring with a MANDATORY 2px
 * background-colored offset — without the offset the ring-vs-primary-fill
 * contrast is 1.24:1 and fails WCAG 1.4.11 (design/08 B-1a). */
export const focusRing =
  "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring " +
  "focus-visible:ring-offset-2 focus-visible:ring-offset-background";
