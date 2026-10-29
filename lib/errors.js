/** Errors surfaced to the model with a stable code prefix. */
export class ComputerUseError extends Error {
  constructor(code, message, options) {
    super(`[${code}] ${message}`, options);
    this.name = 'ComputerUseError';
    this.code = code;
  }
}

export function asComputerUseError(error) {
  if (error instanceof ComputerUseError) return error;
  const message = error instanceof Error ? error.message : String(error);
  const code = error && typeof error === 'object' && typeof error.code === 'string' ? error.code : 'COMPUTER_USE_FAILED';
  return new ComputerUseError(code, message, error instanceof Error ? { cause: error } : undefined);
}
