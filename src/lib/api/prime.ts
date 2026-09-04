import { invoke } from "@tauri-apps/api/core";
import type { UsageScript } from "@/types";

export interface PrimeCurrentState {
  enabledProviderIds: string[];
  defaultProviderId: string | null;
}

export const primeApi = {
  async getCurrentState(): Promise<PrimeCurrentState> {
    return await invoke("get_prime_current_state");
  },

  async updateProviderUsageScript(
    id: string,
    usageScript: UsageScript,
  ): Promise<boolean> {
    return await invoke("update_prime_provider_usage_script", {
      id,
      usageScript,
    });
  },
};
