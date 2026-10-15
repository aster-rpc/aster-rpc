#include "aster_ffi.h"
#include <string.h>

/* Test-only native oracle: signatures compile against the actual public C header. */
uint32_t aster_abi_version(void) { return ASTER_ABI_VERSION; }

/* C ABI 6.0: IrohLibrary calls the handshake on load; the stub accepts its
 * own major. */
int32_t aster_abi_accept(uint32_t expected_abi_major) {
  return expected_abi_major == ASTER_ABI_VERSION_MAJOR ? 0 : -1;
}

int32_t aster_connection_send_datagram(aster_runtime_t runtime,
                                       aster_connection_t connection,
                                       struct aster_bytes_t data) {
  if (runtime != 11) return -102;
  if (connection != 22) return -103;
  if (data.len != 7 || !data.ptr || memcmp(data.ptr, "payload", 7)) return -101;
  return 0;
}

int32_t aster_blobs_add_bytes_as_collection(aster_runtime_t runtime,
                                            aster_node_t node,
                                            struct aster_bytes_t name,
                                            struct aster_bytes_t data,
                                            uint64_t user_data,
                                            aster_operation_t *out_operation) {
  if (runtime != 11) return -102;
  if (node != 22) return -103;
  if (name.len != 7 || !name.ptr || memcmp(name.ptr, "payload", 7)) return -101;
  if (data.len != 7 || !data.ptr || memcmp(data.ptr, "payload", 7)) return -101;
  if (user_data != 0 && user_data != 33) return -104;
  if (!out_operation) return -109;
  return 0;
}

int32_t aster_blobs_list_collection(aster_runtime_t runtime,
                                    aster_node_t node,
                                    struct aster_bytes_t hash_hex,
                                    uint64_t user_data,
                                    aster_operation_t *out_operation) {
  if (runtime != 11) return -102;
  if (node != 22) return -103;
  if (hash_hex.len != 7 || !hash_hex.ptr || memcmp(hash_hex.ptr, "payload", 7)) return -101;
  if (user_data != 0 && user_data != 33) return -104;
  if (!out_operation) return -109;
  return 0;
}

int32_t aster_blobs_create_ticket(aster_runtime_t runtime,
                                  aster_node_t node,
                                  struct aster_bytes_t hash_hex,
                                  uint8_t *out_buf,
                                  uintptr_t capacity,
                                  uintptr_t *out_len) {
  if (runtime != 11) return -102;
  if (node != 22) return -103;
  if (hash_hex.len != 7 || !hash_hex.ptr || memcmp(hash_hex.ptr, "payload", 7)) return -101;
  if (!out_buf) return -109;
  if (capacity != 64) return -110;
  if (!out_len) return -109;
  return 0;
}

int32_t aster_blobs_create_collection_ticket(aster_runtime_t runtime,
                                             aster_node_t node,
                                             struct aster_bytes_t hash_hex,
                                             uint8_t *out_buf,
                                             uintptr_t capacity,
                                             uintptr_t *out_len) {
  if (runtime != 11) return -102;
  if (node != 22) return -103;
  if (hash_hex.len != 7 || !hash_hex.ptr || memcmp(hash_hex.ptr, "payload", 7)) return -101;
  if (!out_buf) return -109;
  if (capacity != 64) return -110;
  if (!out_len) return -109;
  return 0;
}

int32_t aster_blobs_download(aster_runtime_t runtime,
                             aster_node_t node,
                             struct aster_bytes_t ticket,
                             uint64_t user_data,
                             aster_operation_t *out_operation) {
  if (runtime != 11) return -102;
  if (node != 22) return -103;
  if (ticket.len != 7 || !ticket.ptr || memcmp(ticket.ptr, "payload", 7)) return -101;
  if (user_data != 0 && user_data != 33) return -104;
  if (!out_operation) return -109;
  return 0;
}

int32_t aster_blobs_add_path(aster_runtime_t runtime,
                             aster_node_t node,
                             struct aster_bytes_t path,
                             uint64_t user_data,
                             aster_operation_t *out_operation) {
  if (runtime != 11) return -102;
  if (node != 22) return -103;
  if (path.len != 7 || !path.ptr || memcmp(path.ptr, "payload", 7)) return -101;
  if (user_data != 0 && user_data != 33) return -104;
  if (!out_operation) return -109;
  return 0;
}

int32_t aster_blobs_add_path_with_named_tag(aster_runtime_t runtime,
                                            aster_node_t node,
                                            struct aster_bytes_t path,
                                            struct aster_bytes_t tag_name,
                                            uint64_t user_data,
                                            aster_operation_t *out_operation) {
  if (runtime != 11) return -102;
  if (node != 22) return -103;
  if (path.len != 7 || !path.ptr || memcmp(path.ptr, "payload", 7)) return -101;
  if (tag_name.len != 7 || !tag_name.ptr || memcmp(tag_name.ptr, "payload", 7)) return -101;
  if (user_data != 0 && user_data != 33) return -104;
  if (!out_operation) return -109;
  return 0;
}

int32_t aster_blobs_add_collection(aster_runtime_t runtime,
                                   aster_node_t node,
                                   struct aster_bytes_t entries_json,
                                   uint64_t user_data,
                                   aster_operation_t *out_operation) {
  if (runtime != 11) return -102;
  if (node != 22) return -103;
  if (entries_json.len != 7 || !entries_json.ptr || memcmp(entries_json.ptr, "payload", 7)) return -101;
  if (user_data != 0 && user_data != 33) return -104;
  if (!out_operation) return -109;
  return 0;
}

int32_t aster_registry_resolve(aster_runtime_t runtime,
                               uint64_t doc,
                               struct aster_bytes_t opts_json,
                               uint64_t user_data,
                               aster_operation_t *out_operation) {
  if (runtime != 11) return -102;
  if (doc != 22) return -103;
  if (opts_json.len != 7 || !opts_json.ptr || memcmp(opts_json.ptr, "payload", 7)) return -101;
  if (user_data != 0 && user_data != 33) return -104;
  if (!out_operation) return -109;
  return 0;
}

int32_t aster_registry_publish(aster_runtime_t runtime,
                               uint64_t doc,
                               struct aster_bytes_t author_id,
                               struct aster_bytes_t lease_json,
                               struct aster_bytes_t artifact_json,
                               struct aster_bytes_t service,
                               int32_t version,
                               struct aster_bytes_t channel,
                               uint64_t gossip_topic,
                               uint64_t user_data,
                               aster_operation_t *out_operation) {
  if (runtime != 11) return -102;
  if (doc != 22) return -103;
  if (author_id.len != 7 || !author_id.ptr || memcmp(author_id.ptr, "payload", 7)) return -101;
  if (lease_json.len != 7 || !lease_json.ptr || memcmp(lease_json.ptr, "payload", 7)) return -101;
  if (artifact_json.len != 7 || !artifact_json.ptr || memcmp(artifact_json.ptr, "payload", 7)) return -101;
  if (service.len != 7 || !service.ptr || memcmp(service.ptr, "payload", 7)) return -101;
  if (version != 4) return -105;
  if (channel.len != 7 || !channel.ptr || memcmp(channel.ptr, "payload", 7)) return -101;
  if (gossip_topic != 44) return -106;
  if (user_data != 0 && user_data != 33) return -104;
  if (!out_operation) return -109;
  return 0;
}

int32_t aster_registry_renew_lease(aster_runtime_t runtime,
                                   uint64_t doc,
                                   struct aster_bytes_t author_id,
                                   struct aster_bytes_t service,
                                   struct aster_bytes_t contract_id,
                                   struct aster_bytes_t endpoint_id,
                                   struct aster_bytes_t health,
                                   float load,
                                   int32_t lease_duration_s,
                                   uint64_t gossip_topic,
                                   uint64_t user_data,
                                   aster_operation_t *out_operation) {
  if (runtime != 11) return -102;
  if (doc != 22) return -103;
  if (author_id.len != 7 || !author_id.ptr || memcmp(author_id.ptr, "payload", 7)) return -101;
  if (service.len != 7 || !service.ptr || memcmp(service.ptr, "payload", 7)) return -101;
  if (contract_id.len != 7 || !contract_id.ptr || memcmp(contract_id.ptr, "payload", 7)) return -101;
  if (endpoint_id.len != 7 || !endpoint_id.ptr || memcmp(endpoint_id.ptr, "payload", 7)) return -101;
  if (health.len != 7 || !health.ptr || memcmp(health.ptr, "payload", 7)) return -101;
  if (load != 0.5f) return -107;
  if (lease_duration_s != 55) return -108;
  if (gossip_topic != 44) return -106;
  if (user_data != 0 && user_data != 33) return -104;
  if (!out_operation) return -109;
  return 0;
}

int32_t aster_registry_acl_add_writer(aster_runtime_t runtime,
                                      uint64_t doc,
                                      struct aster_bytes_t author_id,
                                      struct aster_bytes_t writer_id,
                                      uint64_t user_data,
                                      aster_operation_t *out_operation) {
  if (runtime != 11) return -102;
  if (doc != 22) return -103;
  if (author_id.len != 7 || !author_id.ptr || memcmp(author_id.ptr, "payload", 7)) return -101;
  if (writer_id.len != 7 || !writer_id.ptr || memcmp(writer_id.ptr, "payload", 7)) return -101;
  if (user_data != 0 && user_data != 33) return -104;
  if (!out_operation) return -109;
  return 0;
}

int32_t aster_registry_acl_remove_writer(aster_runtime_t runtime,
                                         uint64_t doc,
                                         struct aster_bytes_t author_id,
                                         struct aster_bytes_t writer_id,
                                         uint64_t user_data,
                                         aster_operation_t *out_operation) {
  if (runtime != 11) return -102;
  if (doc != 22) return -103;
  if (author_id.len != 7 || !author_id.ptr || memcmp(author_id.ptr, "payload", 7)) return -101;
  if (writer_id.len != 7 || !writer_id.ptr || memcmp(writer_id.ptr, "payload", 7)) return -101;
  if (user_data != 0 && user_data != 33) return -104;
  if (!out_operation) return -109;
  return 0;
}
