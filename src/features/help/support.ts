// Help form model: fields, validation and the request sent to `support_send`.
// Limits and the email check mirror src-tauri/src/commands/support.rs.

import type { SupportRequest, SupportTopic } from '@/lib/types';

export const TOPICS: SupportTopic[] = ['bug', 'feature', 'question', 'other'];

export const LIMITS = { name: 100, subject: 200, message: 10_000 } as const;

export interface SupportForm {
  name: string;
  email: string;
  topic: SupportTopic | '';
  subject: string;
  message: string;
  includeSystem: boolean;
}

export type FieldName = 'name' | 'email' | 'topic' | 'subject' | 'message';
/** Key under `help:error.*`. */
export type FieldError = 'required' | 'email' | 'topic' | 'tooLong';
export type FormErrors = Partial<Record<FieldName, FieldError>>;

const CONTACT_KEY = 'dbstudio.support.contact';

/** Name and email of the last sent request, so they are typed once. */
export function rememberedContact(): { name: string; email: string } {
  try {
    const raw = JSON.parse(localStorage.getItem(CONTACT_KEY) ?? '{}') as Record<string, unknown>;
    return {
      name: typeof raw.name === 'string' ? raw.name : '',
      email: typeof raw.email === 'string' ? raw.email : '',
    };
  } catch {
    return { name: '', email: '' };
  }
}

export function rememberContact(name: string, email: string): void {
  try {
    localStorage.setItem(CONTACT_KEY, JSON.stringify({ name, email }));
  } catch {
    // storage unavailable
  }
}

export const emptyForm = (contact = rememberedContact()): SupportForm => ({
  ...contact,
  topic: '',
  subject: '',
  message: '',
  includeSystem: true,
});

/** Loose `local@domain.tld` check; the same rule as `valid_email` in `support.rs`. */
export function isValidEmail(s: string): boolean {
  const v = s.trim();
  const at = v.indexOf('@');
  if (at <= 0 || /\s/.test(v)) return false;
  const domain = v.slice(at + 1);
  if (domain.includes('@')) return false;
  const dot = domain.lastIndexOf('.');
  return dot > 0 && dot < domain.length - 1;
}

export function validate(f: SupportForm): FormErrors {
  const e: FormErrors = {};
  const tooLong = (v: string, max: number) => [...v.trim()].length > max;
  if (!f.name.trim()) e.name = 'required';
  else if (tooLong(f.name, LIMITS.name)) e.name = 'tooLong';
  if (!f.email.trim()) e.email = 'required';
  else if (!isValidEmail(f.email)) e.email = 'email';
  if (!f.topic) e.topic = 'topic';
  if (!f.subject.trim()) e.subject = 'required';
  else if (tooLong(f.subject, LIMITS.subject)) e.subject = 'tooLong';
  if (!f.message.trim()) e.message = 'required';
  else if (tooLong(f.message, LIMITS.message)) e.message = 'tooLong';
  return e;
}

/** The request for a form that passed `validate`. */
export function toRequest(f: SupportForm): SupportRequest {
  return {
    name: f.name.trim(),
    email: f.email.trim(),
    topic: f.topic || 'other',
    subject: f.subject.trim(),
    message: f.message.trim(),
    includeSystem: f.includeSystem,
  };
}
