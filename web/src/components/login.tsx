/** 多租户登录页：密码登录、用户名登录和个人 Token 登录。 */
import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Mail, Loader2, UserPlus, KeyRound } from 'lucide-react';

interface Props {
  onLogin: (identifier: string, password: string) => Promise<boolean>;
  onTokenLogin?: (token: string) => Promise<boolean>;
  onRegister?: (username: string, email: string, password: string) => Promise<boolean>;
}

export function LoginPage({ onLogin, onTokenLogin, onRegister }: Props) {
  const { t } = useTranslation();
  const [mode, setMode] = useState<'login' | 'register' | 'token'>('login');
  const [identifier, setIdentifier] = useState('');
  const [username, setUsername] = useState('');
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setLoading(true); setError('');
    try {
      const ok = mode === 'token'
        ? await onTokenLogin?.(identifier.trim())
        : mode === 'register'
          ? await onRegister?.(username.trim(), email.trim(), password.trim())
          : await onLogin(identifier.trim(), password.trim());
      if (!ok) setError(t('Login failed'));
    } catch { setError(t('Failed to connect to server')); }
    finally { setLoading(false); }
  };
  const canSubmit = mode === 'token' ? identifier.trim() : mode === 'register' ? username.trim() && email.trim() && password.trim() : identifier.trim() && password.trim();
  return (
    <div className="relative flex h-screen items-center justify-center overflow-hidden bg-background">
      <form onSubmit={handleSubmit} className="glass-5 relative z-10 w-full max-w-sm space-y-5 rounded-3xl p-8">
        <div className="flex items-center gap-2.5 text-lg font-semibold"><Mail className="h-5 w-5 opacity-70" />Codex WebUI</div>
        <p className="text-sm text-muted-foreground">{mode === 'token' ? t('Enter your login token.') : mode === 'login' ? t('Enter your email, username and password to continue.') : t('Create a new account.')}</p>
        {mode === 'register' && <Input placeholder={t('Username')} value={username} onChange={(e) => setUsername(e.target.value)} />}
        {mode !== 'register' && <Input placeholder={mode === 'token' ? t('Login token') : t('Email or username')} value={identifier} onChange={(e) => setIdentifier(e.target.value)} />}
        {mode === 'register' && <Input type="email" placeholder={t('Email')} value={email} onChange={(e) => setEmail(e.target.value)} />}
        {mode !== 'token' && <Input type="password" placeholder={t('Password')} value={password} onChange={(e) => setPassword(e.target.value)} />}
        {error && <div className="rounded-lg bg-red-500/10 p-3 text-sm text-red-500">{error}</div>}
        <Button type="submit" className="w-full rounded-xl" disabled={loading || !canSubmit}>{loading ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : null}{mode === 'register' ? t('Create account') : mode === 'token' ? t('Sign in with token') : t('Sign in')}</Button>
        {onTokenLogin && <button type="button" onClick={() => setMode(mode === 'token' ? 'login' : 'token')} className="flex w-full items-center justify-center gap-2 text-sm text-muted-foreground hover:text-foreground"><KeyRound className="h-4 w-4" />{mode === 'token' ? t('Use password') : t('Use login token')}</button>}
        {onRegister && <button type="button" onClick={() => setMode(mode === 'register' ? 'login' : 'register')} className="flex w-full items-center justify-center gap-2 text-sm text-muted-foreground hover:text-foreground"><UserPlus className="h-4 w-4" />{mode === 'register' ? t('Sign in instead') : t('Create account')}</button>}
      </form>
    </div>
  );
}
