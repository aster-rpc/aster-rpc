package com.aster.exception;

import com.aster.ffi.IrohException;
import com.aster.ffi.IrohStatus;

/** Thrown when a requested resource (endpoint, connection, node, etc.) is not found. */
public class IrohNotFoundException extends IrohException {
  public IrohNotFoundException(String message) {
    super(IrohStatus.NOT_FOUND, message);
  }
}
