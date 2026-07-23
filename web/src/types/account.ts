/**
 * Account-related notification payload types (server-push, not in generated SDK).
 *
 * TODO: AccountDto 来自旧 OpenAPI SDK,已下线。当前用本地最小形状替代。
 */

/** 账户 DTO 的本地最小形状。 */
interface AccountDto {
  planType?: string | null;
}

export type PlanType = NonNullable<AccountDto['planType']>;
export type AuthMode = 'apikey' | 'chatgpt' | 'chatgptAuthTokens';

export interface AccountLoginCompletedNotification {
  loginId: string | null;
  success: boolean;
  error: string | null;
}

export interface AccountUpdatedNotification {
  authMode: AuthMode | null;
  planType: PlanType | null;
}
