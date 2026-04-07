/**
 * Registry ACL — access control list for registry doc entries.
 *
 * Mirrors bindings/python/aster/registry/acl.py.
 *
 * Starts in open mode (all authors trusted). When the first ACL
 * entry is set, switches to restricted mode.
 */

import { aclKey } from './keys.js';

/** Doc handle interface for ACL operations. */
interface AclDoc {
  setBytes(authorHex: string, key: string, value: Uint8Array): Promise<string>;
  getExact(authorHex: string, key: string): Promise<Uint8Array | null>;
}

/**
 * Registry ACL — filters doc entries by trusted authors.
 */
export class RegistryACL {
  private writers = new Set<string>();
  private readers = new Set<string>();
  private admins = new Set<string>();
  private _restricted = false;

  /** Whether ACL is in restricted mode. */
  get restricted(): boolean {
    return this._restricted;
  }

  /** Check if an author is a trusted writer. */
  isTrustedWriter(authorId: string): boolean {
    if (!this._restricted) return true;
    return this.writers.has(authorId) || this.admins.has(authorId);
  }

  /** Check if an author is a trusted reader. */
  isTrustedReader(authorId: string): boolean {
    if (!this._restricted) return true;
    return this.readers.has(authorId) || this.writers.has(authorId) || this.admins.has(authorId);
  }

  /**
   * Filter entries to only those from trusted authors.
   *
   * @param entries - Array of [authorId, value] pairs
   * @returns Filtered entries
   */
  filterTrusted<T extends { authorId: string }>(entries: T[]): T[] {
    if (!this._restricted) return entries;
    return entries.filter(e => this.isTrustedWriter(e.authorId));
  }

  /** Add a writer and switch to restricted mode. */
  async addWriter(authorId: string, doc?: AclDoc, adminAuthorId?: string): Promise<void> {
    this.writers.add(authorId);
    this._restricted = true;

    if (doc && adminAuthorId) {
      const encoder = new TextEncoder();
      const data = JSON.stringify([...this.writers]);
      await doc.setBytes(adminAuthorId, aclKey('writers'), encoder.encode(data));
    }
  }

  /** Add a reader. */
  addReader(authorId: string): void {
    this.readers.add(authorId);
    this._restricted = true;
  }

  /** Add an admin. */
  addAdmin(authorId: string): void {
    this.admins.add(authorId);
    this._restricted = true;
  }

  /** Remove a writer from the ACL. */
  removeWriter(authorId: string): void {
    this.writers.delete(authorId);
  }

  /** Get all writers. */
  getWriters(): string[] {
    return [...this.writers];
  }

  /** Get all readers. */
  getReaders(): string[] {
    return [...this.readers];
  }

  /** Get all admins. */
  getAdmins(): string[] {
    return [...this.admins];
  }

  /**
   * Reload ACL state from the registry doc.
   */
  async reload(doc: AclDoc, authorId: string): Promise<void> {
    const decoder = new TextDecoder();

    const writersData = await doc.getExact(authorId, aclKey('writers'));
    if (writersData) {
      const list: string[] = JSON.parse(decoder.decode(writersData));
      this.writers = new Set(list);
      this._restricted = true;
    }

    const readersData = await doc.getExact(authorId, aclKey('readers'));
    if (readersData) {
      const list: string[] = JSON.parse(decoder.decode(readersData));
      this.readers = new Set(list);
    }

    const adminsData = await doc.getExact(authorId, aclKey('admins'));
    if (adminsData) {
      const list: string[] = JSON.parse(decoder.decode(adminsData));
      this.admins = new Set(list);
    }
  }
}
