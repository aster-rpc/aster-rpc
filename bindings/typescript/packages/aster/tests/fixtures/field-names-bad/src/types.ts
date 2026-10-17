import { WireType, WireName } from '@aster-rpc/aster';

// aster-gen refuses this module: the override's snake_case form (written_by)
// is not the property's (author), so Fory would write a name no other binding
// expects (ffi_spec/Aster-FieldNaming.md §4).

@WireType('names.v1/Bad')
export class Bad {
  @WireName('writtenBy') author = '';
}
