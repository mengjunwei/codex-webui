/**
 * Manages @ mention popover state: detection, navigation, selection.
 * Used by ChatInput to drive the MentionPopover component.
 */
import { useCallback, useRef, useState } from 'react';
import type { MentionResult } from '@/components/chat/mention-popover';
import { escapeMentionPath } from '@/lib/mention-utils';
import type { ChatFileAttachment } from '@/types/attachments';

let mentionIdCounter = 0;
function nextMentionId(): string {
  return `att-${++mentionIdCounter}-${Date.now()}`;
}

/** Returns true when a string contains whitespace that is not escaped as `\ `. */
function hasUnescapedWhitespace(value: string): boolean {
  for (let i = 0; i < value.length; i++) {
    if (/\s/.test(value[i]) && value[i - 1] !== '\\') return true;
  }
  return false;
}

interface UseChatMentionParams {
  textareaRef: React.RefObject<HTMLTextAreaElement | null>;
  valueRef: React.RefObject<string>;
  setValue: React.Dispatch<React.SetStateAction<string>>;
  setAttachments: React.Dispatch<React.SetStateAction<import('@/types/attachments').ChatAttachment[]>>;
  toRelativePath: (absolutePath: string) => string;
}

export function useChatMention({
  textareaRef,
  valueRef,
  setValue,
  setAttachments,
  toRelativePath,
}: UseChatMentionParams) {
  const [mentionOpen, setMentionOpen] = useState(false);
  const [mentionQuery, setMentionQuery] = useState('');
  const [mentionStart, setMentionStart] = useState(-1);
  const [mentionSelectedIndex, setMentionSelectedIndex] = useState(0);
  const mentionFilteredRef = useRef<MentionResult[]>([]);

  /** Called on every textarea value change to detect ` @` trigger. */
  const detectMention = useCallback((newValue: string) => {
    const textarea = textareaRef.current;
    if (!textarea) return;
    const cursorPos = textarea.selectionStart;
    const textBeforeCursor = newValue.slice(0, cursorPos);
    const atIndex = textBeforeCursor.lastIndexOf('@');

    if (atIndex >= 0) {
      // Only trigger when @ is preceded by space/newline or at start
      const charBefore = atIndex > 0 ? textBeforeCursor[atIndex - 1] : ' ';
      if (charBefore === ' ' || charBefore === '\n' || atIndex === 0) {
        const q = textBeforeCursor.slice(atIndex + 1);
        // Allow / and escaped spaces for path navigation; close on unescaped whitespace
        if (!hasUnescapedWhitespace(q)) {
          setMentionOpen(true);
          setMentionQuery(q);
          setMentionStart(atIndex);
          if (!q.endsWith('/')) setMentionSelectedIndex(0);
          return;
        }
      }
    }
    setMentionOpen(false);
  }, [textareaRef]);

  /** File selected from @ popover: insert @relative/path at the @ position. */
  const handleMentionSelect = useCallback((result: MentionResult) => {
    const currentValue = valueRef.current;
    const relativePath = toRelativePath(result.path);
    const escapedPath = escapeMentionPath(relativePath);
    const before = currentValue.slice(0, mentionStart);
    const after = currentValue.slice(mentionStart + 1 + mentionQuery.length);
    const inserted = `@${escapedPath} `;
    setValue(before + inserted + after);
    setMentionOpen(false);

    // displayName stores the escaped form for matching in buildInput
    setAttachments((prev) => [
      ...prev,
      { type: 'mention', id: nextMentionId(), displayName: escapedPath, path: result.path } as ChatFileAttachment,
    ]);

    const newPos = mentionStart + inserted.length;
    setTimeout(() => {
      textareaRef.current?.focus();
      textareaRef.current?.setSelectionRange(newPos, newPos);
    }, 0);
  }, [mentionStart, mentionQuery, toRelativePath, valueRef, setValue, setAttachments, textareaRef]);

  /** Navigate into a directory: update textarea query to dir path + trailing slash. */
  const handleMentionNavigate = useCallback((dirPath: string) => {
    const currentValue = valueRef.current;
    const relativePath = toRelativePath(dirPath);
    const before = currentValue.slice(0, mentionStart + 1);
    const after = currentValue.slice(mentionStart + 1 + mentionQuery.length);
    const newQuery = `${escapeMentionPath(relativePath)}/`;
    setValue(before + newQuery + after);
    setMentionQuery(newQuery);
    setMentionSelectedIndex(0);
    setTimeout(() => textareaRef.current?.focus(), 0);
  }, [mentionStart, mentionQuery, toRelativePath, valueRef, setValue, textareaRef]);

  /** Navigate up to a breadcrumb segment. */
  const handleMentionNavigateUp = useCallback((relativePath: string) => {
    const currentValue = valueRef.current;
    const before = currentValue.slice(0, mentionStart + 1);
    const after = currentValue.slice(mentionStart + 1 + mentionQuery.length);
    const newQuery = relativePath ? `${relativePath}/` : '';
    setValue(before + newQuery + after);
    setMentionQuery(newQuery);
    setMentionSelectedIndex(0);
    setTimeout(() => textareaRef.current?.focus(), 0);
  }, [mentionStart, mentionQuery, valueRef, setValue, textareaRef]);

  /** Handle keyboard events when mention popover is open. Returns true if handled. */
  const handleMentionKeyDown = useCallback((e: React.KeyboardEvent): boolean => {
    if (!mentionOpen) return false;

    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setMentionSelectedIndex((i) => Math.min(i + 1, mentionFilteredRef.current.length - 1));
      return true;
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      setMentionSelectedIndex((i) => Math.max(i - 1, 0));
      return true;
    }
    if (e.key === 'ArrowRight') {
      const item = mentionFilteredRef.current[mentionSelectedIndex];
      if (item?.type === 'directory') {
        e.preventDefault();
        handleMentionNavigate(item.path);
        return true;
      }
    }
    if (e.key === 'Enter' || e.key === 'Tab') {
      e.preventDefault();
      const item = mentionFilteredRef.current[mentionSelectedIndex];
      if (item) {
        if (item.type === 'directory') {
          handleMentionNavigate(item.path);
        } else {
          handleMentionSelect(item);
        }
      }
      return true;
    }
    if (e.key === 'Escape') {
      e.preventDefault();
      setMentionOpen(false);
      return true;
    }
    return false;
  }, [mentionOpen, mentionSelectedIndex, handleMentionSelect, handleMentionNavigate]);

  return {
    mentionOpen,
    mentionQuery,
    mentionSelectedIndex,
    mentionFilteredRef,
    setMentionOpen,
    detectMention,
    handleMentionSelect,
    handleMentionNavigate,
    handleMentionNavigateUp,
    handleMentionKeyDown,
  };
}
