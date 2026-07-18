/** Thin adapter over generated SDK for runtime settings endpoints. */
import {
  list as settingsListSettings,
  updateOne as settingsUpdateSetting,
  deleteOne as settingsResetSetting,
} from '@/generated/api/sdk.gen';
import type { SettingDto } from '@/generated/api/types.gen';

// TODO: SettingConstraintsDto / UpdateSettingDto 来自旧 OpenAPI SDK,已下线。
//       当前用本地最小形状替代。
export interface SettingConstraints {
  [key: string]: unknown;
}
export interface UpdateSettingDto {
  value: unknown;
}

export type SettingType = SettingDto['type'];
export type SettingCategory = SettingDto['category'];
export type SettingSource = SettingDto['source'];
export type SettingValue = SettingDto['value'];
export type SettingConstraintsAlias = SettingConstraints;

/** Frontend-friendly alias for the generated SettingDto. */
export type RuntimeSetting = SettingDto;

export interface SettingsListResponse {
  settings: RuntimeSetting[];
}

export const settingsQueryKey = (category: SettingCategory) =>
  ['settings', category] as const;

/** Lists runtime settings by category. */
export async function listSettings(
  category: SettingCategory,
): Promise<SettingsListResponse> {
  const { data } = await settingsListSettings({
    query: { category },
    throwOnError: true,
  });
  return data;
}

/** Updates one runtime setting; value=null resets to env/default fallback. */
export async function updateSetting(
  key: string,
  value: SettingValue | null,
): Promise<RuntimeSetting> {
  const body: UpdateSettingDto = { value };
  const { data } = await settingsUpdateSetting({
    path: { key },
    body,
    throwOnError: true,
  });
  return data;
}

/** Clears one runtime setting override (resets to env/default). */
export async function resetSetting(key: string): Promise<RuntimeSetting> {
  const { data } = await settingsResetSetting({
    path: { key },
    throwOnError: true,
  });
  return data;
}
