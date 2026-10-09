class AgentLauncher < Formula
  desc "Terminal inbox that dispatches coding agents to a repository's issues and PRs"
  homepage "https://github.com/penso/agent-launcher"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/penso/agent-launcher/releases/download/v@VERSION@/agent-launcher-@VERSION@-aarch64-apple-darwin.tar.gz"
      sha256 "@SHA256_MACOS_AARCH64@"
    end
    on_intel do
      url "https://github.com/penso/agent-launcher/releases/download/v@VERSION@/agent-launcher-@VERSION@-x86_64-apple-darwin.tar.gz"
      sha256 "@SHA256_MACOS_X86_64@"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/penso/agent-launcher/releases/download/v@VERSION@/agent-launcher-@VERSION@-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "@SHA256_LINUX_AARCH64@"
    end
    on_intel do
      url "https://github.com/penso/agent-launcher/releases/download/v@VERSION@/agent-launcher-@VERSION@-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "@SHA256_LINUX_X86_64@"
    end
  end

  def install
    bin.install "bin/agent-launcher"
    pkgshare.install "config.example.toml"
    doc.install "README.md", "NOTICE", "THIRD-PARTY-NOTICES.txt"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/agent-launcher --version")
  end
end
