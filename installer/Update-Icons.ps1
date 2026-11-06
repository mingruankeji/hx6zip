param()

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$assets = Join-Path (Split-Path -Parent $PSScriptRoot) 'assets'
$source = [System.Drawing.Image]::FromFile((Join-Path $assets 'mzip.png'))

function Get-IconPng([int]$Size) {
    $bitmap = New-Object System.Drawing.Bitmap($Size, $Size)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $attributes = New-Object System.Drawing.Imaging.ImageAttributes
    $stream = New-Object System.IO.MemoryStream
    try {
        $graphics.Clear([System.Drawing.Color]::Transparent)
        $graphics.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $attributes.SetWrapMode([System.Drawing.Drawing2D.WrapMode]::TileFlipXY)
        $rectangle = New-Object System.Drawing.Rectangle(0, 0, $Size, $Size)
        $graphics.DrawImage($source, $rectangle, 0, 0, $source.Width, $source.Height,
            [System.Drawing.GraphicsUnit]::Pixel, $attributes)
        $bitmap.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
        return ,$stream.ToArray()
    } finally {
        $stream.Dispose()
        $attributes.Dispose()
        $graphics.Dispose()
        $bitmap.Dispose()
    }
}

try {
    $sizes = @(16, 20, 24, 32, 40, 48, 64, 96, 128, 256)
    $frames = @($sizes | ForEach-Object { Get-IconPng $_ })
    $stream = [System.IO.File]::Create((Join-Path $assets 'mzip.ico'))
    $writer = New-Object System.IO.BinaryWriter($stream)
    try {
        $writer.Write([uint16]0)
        $writer.Write([uint16]1)
        $writer.Write([uint16]$sizes.Count)
        $offset = 6 + 16 * $sizes.Count
        for ($i = 0; $i -lt $sizes.Count; $i++) {
            $dimension = $sizes[$i] % 256
            $writer.Write([byte]$dimension)
            $writer.Write([byte]$dimension)
            $writer.Write([byte]0)
            $writer.Write([byte]0)
            $writer.Write([uint16]1)
            $writer.Write([uint16]32)
            $writer.Write([uint32]$frames[$i].Length)
            $writer.Write([uint32]$offset)
            $offset += $frames[$i].Length
        }
        foreach ($frame in $frames) {
            $writer.Write([byte[]]$frame)
        }
    } finally {
        $writer.Dispose()
        $stream.Dispose()
    }

    foreach ($logo in @(
        @{ Name = 'StoreLogo.png'; Size = 50 },
        @{ Name = 'Square44x44Logo.png'; Size = 44 },
        @{ Name = 'Square150x150Logo.png'; Size = 150 }
    )) {
        [System.IO.File]::WriteAllBytes((Join-Path $assets $logo.Name), (Get-IconPng $logo.Size))
    }
} finally {
    $source.Dispose()
}

Write-Output "Updated MZip ICO and package logos in $assets"
