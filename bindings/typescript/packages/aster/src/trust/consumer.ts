/**
 * Consumer admission — the client-side admission handshake.
 *
 * Spec reference: Aster-trust-spec.md S5
 *
 * When a consumer connects to a producer, it must:
 * 1. Open a stream on the admission ALPN
 * 2. Send a ConsumerAdmissionRequest (credential + optional IID token)
 * 3. Receive a ConsumerAdmissionResponse (services list + registry ticket)
 */

import type { ConsumerEnrollmentCredential } from './credentials.js';
import { MAX_ADMISSION_PAYLOAD_SIZE, MAX_SERVICES_IN_ADMISSION } from '../limits.js';

/** Service summary returned in admission response. */
export interface ServiceSummary {
  name: string;
  version: number;
  contractId: string;
  pattern: string;
  methods: string[];
}

/** Consumer admission request. */
export interface ConsumerAdmissionRequest {
  credentialJson: string;
  iidToken?: string;
}

/** Consumer admission response from producer. */
export interface ConsumerAdmissionResponse {
  admitted: boolean;
  reason?: string;
  services: ServiceSummary[];
  registryTicket?: string;
  attributes?: Record<string, string>;
}

/**
 * Perform the consumer admission handshake.
 *
 * @param connection - The QUIC connection to the producer (admission ALPN)
 * @param credential - The consumer enrollment credential
 * @param iidToken - Optional cloud instance identity token
 * @returns The admission response with services and registry ticket
 */
export async function performAdmission(
  connection: { openBi(): Promise<{ takeSend(): any; takeRecv(): any }> },
  credential: ConsumerEnrollmentCredential,
  iidToken?: string,
): Promise<ConsumerAdmissionResponse> {
  const bi = await connection.openBi();
  const send = bi.takeSend();
  const recv = bi.takeRecv();

  // Build and send request
  const request: ConsumerAdmissionRequest = {
    credentialJson: JSON.stringify(credential),
    iidToken,
  };
  const reqBytes = new TextEncoder().encode(JSON.stringify(request));
  if (reqBytes.byteLength > MAX_ADMISSION_PAYLOAD_SIZE) {
    throw new Error(`admission request too large: ${reqBytes.byteLength} > ${MAX_ADMISSION_PAYLOAD_SIZE}`);
  }

  // Write request + finish send side
  await send.writeAll(reqBytes);
  await send.finish();

  // Read response
  const respBytes = await recv.readToEnd(MAX_ADMISSION_PAYLOAD_SIZE);
  const response: ConsumerAdmissionResponse = JSON.parse(
    new TextDecoder().decode(respBytes),
  );

  // Validate
  if (response.services && response.services.length > MAX_SERVICES_IN_ADMISSION) {
    throw new Error(`admission response has ${response.services.length} services, max is ${MAX_SERVICES_IN_ADMISSION}`);
  }

  return response;
}
