class Chunkloader < Formula
  desc "Dump webpack / Next.js / Framer / Flutter JS chunks and assets for analysis"
  homepage "https://github.com/Sn0wAlice/chunkloader"
  version "0.1.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-darwin-arm64.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-linux-amd64.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    elsif Hardware::CPU.arm?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-linux-arm64.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "chunkloader"
  end

  test do
    assert_match "Usage", shell_output("#{bin}/chunkloader --help")
  end
end
