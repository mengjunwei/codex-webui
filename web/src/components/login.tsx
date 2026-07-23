/**
 * Login + Register page for multi-tenant email + password authentication.
 * Full-screen animated gradient background + glass card.
 */
import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Mail, Loader2, UserPlus } from 'lucide-react';

interface Props {
  onLogin: (email: string, password: string) => Promise<boolean>;
  onRegister?: (email: string, password: string) => Promise<boolean>;
}

export function LoginPage({ onLogin, onRegister }: Props) {
  const { t } = useTranslation();
  const [mode, setMode] = useState<'login' | 'register'>('login');
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
      if (mode === 'register' && onRegister) {
        const ok = await onRegister(trimmedEmail, trimmedPassword);
        if (!ok) setError(t('Registration failed'));
      } else {
        const ok = await onLogin(trimmedEmail, trimmedPassword);
        if (!ok) setError(t('Invalid email or password'));
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
          {mode === 'login'
            ? t('Enter your email and password to continue.')
            : t('Create a new account.')}
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
          {mode === 'login' ? t('Sign in') : t('Create account')}
        </Button>
        {onRegister && (
          <button
            type="button"
            onClick={() => setMode(mode === 'login' ? 'register' : 'login')}
            className="flex w-full items-center justify-center gap-2 text-sm text-muted-foreground hover:text-foreground"
          >
            <UserPlus className="h-4 w-4" />
            {mode === 'login' ? t('Create account') : t('Sign in instead')}
          </button>
        )}
      </form>
    </div>
  );
}