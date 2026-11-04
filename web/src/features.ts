// What services' operator turned on (`/api/features`), so the page offers
// only what works. Services still refuses anything that is off.

import { useEffect, useState } from "react";
import { request } from "./orgs";
import type { Providers } from "./sso";

export interface Features {
  /** Whether services sends email: new accounts confirm their address, and
   * signing in with a password also takes an emailed code. */
  email: boolean;
  /** Whether accounts can turn on two-factor authentication. */
  totp: boolean;
  sso: Providers;
}

export const features = () => request<Features>("GET", "/api/features");

/** The server's features, or null until it says, or if it cannot. */
export function useFeatures(): Features | null {
  const [offered, setOffered] = useState<Features | null>(null);
  useEffect(() => {
    let live = true;
    features().then(
      (answer) => live && setOffered(answer),
      () => {},
    );
    return () => {
      live = false;
    };
  }, []);
  return offered;
}
