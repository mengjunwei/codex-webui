/** Configures the Hey API generated client with auth and error handling. */
import { client } from './generated/api/client.gen';
import { getApiToken, clearApiToken } from './auth-token';

/** Call once at app startup to configure the generated SDK client. */
export function configureApiClient() {
  client.interceptors.request.use((request) => {
    const token = getApiToken();
    if (token) {
      request.headers.set('Authorization', `Bearer ${token}`);
    }
    return request;
  });

  client.interceptors.response.use((response) => {
    if (response.status === 401) {
      clearApiToken();
    }
    return response;
  });
}
