import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { type Applied, type Section, api } from "./api";

/** The items of a settings section, by name. */
export function useSection<T>(section: Section) {
  return useQuery({ queryKey: ["section", section], queryFn: () => api.section<T>(section) });
}

/** After a settings change, everything derived from the settings is stale. */
export function useInvalidateSettings() {
  const queryClient = useQueryClient();
  return () =>
    Promise.all(
      ["settings", "section", "system", "nodes", "addons", "history"].map((key) =>
        queryClient.invalidateQueries({ queryKey: [key] }),
      ),
    );
}

/** A mutation that changes the settings; refreshes what depends on them afterwards. */
export function useSettingsMutation<V>(fn: (variables: V) => Promise<Applied>) {
  const invalidate = useInvalidateSettings();
  return useMutation({ mutationFn: fn, onSuccess: () => invalidate() });
}

/** Splits a comma- or whitespace-separated list. */
export function splitList(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}
