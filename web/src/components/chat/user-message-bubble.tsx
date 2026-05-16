/** User message bubble with styled @mention badges and image previews. */
import { useEffect, useMemo, useState } from 'react';
import { FileText, ImageIcon } from 'lucide-react';
import { getAuthorizationHeader } from '@/auth-token';
import { normalizeMessageMentions, splitMentionSegments } from '@/lib/mention-utils';

interface Props {
  content: string;
  threadCwd: string | null;
  images?: string[];
}

export function UserMessageBubble({ content, threadCwd, images }: Props) {
  const segments = useMemo(() => {
    const normalized = normalizeMessageMentions(content, threadCwd);
    return splitMentionSegments(normalized);
  }, [content, threadCwd]);

  return (
    <div className="text-sm leading-relaxed">
      <div>
        {segments.map((seg, i) =>
          seg.type === 'mention' ? (
            <span
              key={i}
              className="inline-flex items-center gap-1 rounded bg-white/15 px-1.5 py-0.5 font-mono text-[0.85em]"
            >
              <FileText className="inline h-3 w-3 opacity-70" />
              {seg.value}
            </span>
          ) : (
            <span key={i}>{seg.value}</span>
          ),
        )}
      </div>

      {images && images.length > 0 && (
        <div className="mt-2 flex flex-wrap gap-2">
          {images.map((src, i) => (
            <AuthImage key={i} src={src} />
          ))}
        </div>
      )}
    </div>
  );
}

/** Fetches an image with auth header and displays via blob URL. */
function AuthImage({ src }: { src: string }) {
  const isDirectUrl = /^(https?|data|blob):/.test(src);
  const [blobUrl, setBlobUrl] = useState<string | null>(isDirectUrl ? src : null);
  const [error, setError] = useState(false);

  useEffect(() => {
    if (isDirectUrl) return;
    let revoke: string | null = null;
    let cancelled = false;
    const url = `/api/files/download?path=${encodeURIComponent(src)}`;
    const authorization = getAuthorizationHeader();
    fetch(url, { headers: authorization ? { Authorization: authorization } : {} })
      .then((resp) => {
        if (!resp.ok) throw new Error(`${resp.status}`);
        return resp.blob();
      })
      .then((blob) => {
        if (cancelled) return;
        revoke = URL.createObjectURL(blob);
        setBlobUrl(revoke);
      })
      .catch(() => { if (!cancelled) setError(true); });

    return () => { cancelled = true; if (revoke) URL.revokeObjectURL(revoke); };
  }, [src, isDirectUrl]);

  if (error) {
    return (
      <div className="flex h-20 w-20 items-center justify-center rounded-lg border border-white/20 bg-white/5">
        <ImageIcon className="h-5 w-5 opacity-40" />
      </div>
    );
  }
  if (!blobUrl) {
    return (
      <div className="h-20 w-20 animate-pulse rounded-lg border border-white/20 bg-white/10" />
    );
  }
  return (
    <a href={blobUrl} target="_blank" rel="noopener noreferrer" className="block overflow-hidden rounded-lg border border-white/20">
      <img src={blobUrl} alt="Attached image" loading="lazy" className="max-h-40 max-w-xs object-contain" />
    </a>
  );
}
