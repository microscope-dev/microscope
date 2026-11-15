import { useEffect, useState } from "react";

import { request } from "./orgs";
import type { Providers } from "./sso";

export interface Features {
  email: boolean;

  totp: boolean;
  sso: Providers;

  passkeys: boolean;
}

export const features = () => request<Features>("GET", "/api/features");

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
