package com.aster.exception;

import com.aster.ffi.IrohException;
import com.aster.ffi.IrohStatus;

/** Thrown when a connection is refused by the remote peer. */
public class IrohConnectionRefusedException extends IrohException {
  public IrohConnectionRefusedException(String message) {
    super(IrohStatus.CONNECTION_REFUSED, message);
  }
}
