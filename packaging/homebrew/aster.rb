# Aster CLI Homebrew Formula
#
# This formula lives in the homebrew-aster tap repository at:
#   github.com/aster-rpc/homebrew-aster
#
# Users install via:
#   brew tap aster-rpc/aster
#   brew install aster
#
# Layout produced:
#   Cellar/aster/<version>/libexec/  ← entire standalone dist
#   Cellar/aster/<version>/libexec/aster  ← real binary
#   bin/aster                         ← symlink Brew creates from libexec
#
# Update steps when releasing:
#   1. Bump VERSION below.
#   2. Replace SHA256 values with the ones from the release SHA256SUMS file.
#   3. Push to the tap repo. (CI in the main repo can do this automatically.)

class Aster < Formula
  desc "Aster RPC framework command-line tools"
  homepage "https://aster.site"
  version "0.1.2"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/aster-rpc/aster-rpc/releases/download/cli-v#{version}/aster-dist-macos-aarch64.tar.gz"
      sha256 "REPLACE_WITH_MACOS_AARCH64_SHA256"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/aster-rpc/aster-rpc/releases/download/cli-v#{version}/aster-dist-linux-x86_64.tar.gz"
      sha256 "REPLACE_WITH_LINUX_X86_64_SHA256"
    end
    on_arm do
      url "https://github.com/aster-rpc/aster-rpc/releases/download/cli-v#{version}/aster-dist-linux-aarch64.tar.gz"
      sha256 "REPLACE_WITH_LINUX_AARCH64_SHA256"
    end
  end

  def install
    # The tarball expands into `aster-<suffix>/`. Move every file inside the
    # dist into libexec/ so the layout matches what the launcher expects.
    libexec.install Dir["*"]
    # Brew convention: real binaries in libexec, wrappers in bin/.
    # Since `aster` is a self-contained binary that needs its sibling files,
    # symlink directly rather than wrapping with a shell script.
    bin.install_symlink libexec/"aster"
  end

  test do
    # Smoke test: --version should print "aster aster-cli <version>".
    output = shell_output("#{bin}/aster --version")
    assert_match(/aster\b.*#{Regexp.escape(version.to_s)}/, output)
  end
end
