import { describe, expect, it } from 'vitest';
import { initialForm, profileFromForm } from '@/features/connections/connectionForm';
import { mockParseMongoUri } from '@/lib/mock/catalogMongo';
import { uriFormPatch } from './uriForm';

describe('uriFormPatch', () => {
  it('carries every parsed option into the form and the saved profile', async () => {
    const parsed = await mockParseMongoUri(
      'mongodb+srv://ada:pw@cluster.example.net/app?authSource=admin&replicaSet=rs0&appName=dbs',
    );
    expect(parsed.pendingSecretId).toBeNull();
    const form = { ...initialForm(undefined), name: 'x', ...uriFormPatch(parsed) };
    expect(form.options).toEqual({
      srv: 'true',
      authSource: 'admin',
      replicaSet: 'rs0',
      appName: 'dbs',
    });
    expect(form.tlsMode).toBe('require');
    const profile = profileFromForm(form, undefined, () => 'id');
    expect(profile.options).toEqual(form.options);
    expect(profile.tls.mode).toBe('require');
  });

  it('keeps the options of an existing profile when saving it again', async () => {
    const parsed = await mockParseMongoUri('mongodb://h:27017/?directConnection=true');
    const form = initialForm(parsed.profile);
    expect(profileFromForm(form, parsed.profile, () => 'id').options).toEqual({
      directConnection: 'true',
    });
  });
});
