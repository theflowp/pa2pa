class Pa2pa < Formula
  desc "Personal Agent to Personal Agent — SSH-authenticated, E2E encrypted agent messaging"
  homepage "https://github.com/OWNER/pa2pa"
  version "0.1.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/OWNER/pa2pa/releases/download/v#{version}/pa2pa-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_ARM64_SHA256"
    else
      url "https://github.com/OWNER/pa2pa/releases/download/v#{version}/pa2pa-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_X64_SHA256"
    end
  end

  on_linux do
    url "https://github.com/OWNER/pa2pa/releases/download/v#{version}/pa2pa-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "REPLACE_WITH_LINUX_SHA256"
  end

  def install
    bin.install "pa2pa"
    bin.install "pa2pa-pill" if File.exist?("pa2pa-pill")
  end

  test do
    assert_match "pa2pa", shell_output("#{bin}/pa2pa --version")
  end
end
