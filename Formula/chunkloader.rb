class Chunkloader < Formula
  desc "Dump webpack / Next.js / Framer / Flutter JS chunks and assets for analysis"
  homepage "https://github.com/Sn0wAlice/chunkloader"
  version "1.0.1"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-darwin-arm64.tar.gz"
      sha256 "4feb195722707f372e6d6f37c16c580ecc771447e61f10d2994dfcdd8be3a047"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-linux-amd64.tar.gz"
      sha256 "43d9d55966ec9300c13b1bc85011fb3c34db9b9fdaf9824fa25bc81734002d15"
    elsif Hardware::CPU.arm?
      url "https://github.com/Sn0wAlice/chunkloader/releases/download/v#{version}/chunkloader-linux-arm64.tar.gz"
      sha256 "0d79ca6680031191cb40cd4db6d4aaece16e13b5078cc3b33723b7b4c6a99447"
    end
  end

  def install
    bin.install "chunkloader"
  end

  test do
    assert_match "Usage", shell_output("#{bin}/chunkloader --help")
  end
end
