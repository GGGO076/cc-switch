import { useQuery, type QueryClient } from "@tanstack/react-query";
import { primeApi } from "@/lib/api/prime";

export const primeKeys = {
  all: ["prime"] as const,
  currentState: ["prime", "currentState"] as const,
};

export const invalidatePrimeProviderCaches = async (queryClient: QueryClient) => {
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: primeKeys.currentState }),
    queryClient.invalidateQueries({ queryKey: ["providers", "prime"] }),
  ]);
};

export function usePrimeCurrentState(enabled = true) {
  return useQuery({
    queryKey: primeKeys.currentState,
    queryFn: () => primeApi.getCurrentState(),
    enabled,
  });
}
