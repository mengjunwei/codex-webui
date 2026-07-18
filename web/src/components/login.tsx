/**
 * Login page for multi-tenant email + password authentication.
 * Full-screen animated gradient background + glass card.
 */
import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Mail, Loader2 } from 'lucide-react';

interface Props {
  onLogin: (email: string, password: string) => Promise<boolean>;
}

export function LoginPage({ onLogin }: Props) {
  const { t } = useTranslation();
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmedEmail = email.trim();
    const trimmedPassword = password.trim();
    if (!trimmedEmail || !trimmedPassword) return;

    setLoading(true);
    setError('');

    try {
      const ok = await onLogin(trimmedEmail, trimmedPassword);
      if (!ok) {
        setError(t('Invalid email or password'));
      }
    } catch {
      setError(t('Failed to connect to server'));
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="relative flex h-screen items-center justify-center overflow-hidden bg-background">
      <form
        onSubmit={handleSubmit}
        className="glass-5 relative z-10 w-full max-w-sm space-y-5 rounded-3xl p-8"
      >
        <div className="flex items-center gap-2.5 text-lg font-semibold">
          <Mail className="h-5 w-5 opacity-70" />
          Codex WebUI
        </div>
        <p className="text-sm text-muted-foreground">
          {t('Enter your email and password to continue.')}
        </p>

        <Input
          type="email"
          placeholder={t('Email')}
          value={email}
          onChange={(e) => setEmail(e.target.value)}
          className="rounded-xl border-[var(--glass-border)] bg-background/40 backdrop-blur-sm transition-all focus:bg-background/60"
        />
        <Input
          type="password"
          placeholder={t('Password')}
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          className="rounded-xl border-[var(--glass-border)] bg-background/40 backdrop-blur-sm transition-all focus:bg-background/60"
        />
        {error && (
          <div className="rounded-lg bg-red-500/10 p-3 text-sm text-red-500">
            {error}
          </div>
        )}
        <Button
          type="submit"
          className="w-full rounded-xl"
          disabled={loading || !email.trim() || !password.trim()}
        >
          {loading ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : null}
          {t('Sign in')}
        </Button>
      </form>
    </div>
  );
}