# Image Host API by DishIs Technologies

[![Rust](https://img.shields.io/badge/rust-%23000000.svg?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Rocket](https://img.shields.io/badge/Rocket-000000?style=for-the-badge&logo=rocket&logoColor=white)](https://rocket.rs/)
[![MongoDB](https://img.shields.io/badge/MongoDB-%234ea94b.svg?style=for-the-badge&logo=mongodb&logoColor=white)](https://www.mongodb.com/)
[![Docker](https://img.shields.io/badge/docker-%230db7ed.svg?style=for-the-badge&logo=docker&logoColor=white)](https://www.docker.com/)

A simple, high-performance image hosting service built with Rust, the Rocket web framework, and MongoDB. The application is designed to be deployed easily as a set of Docker containers.

It provides a web interface for manual uploads and a JSON API for programmatic integration. Uploaded images are automatically compressed and optimized in the background.

## Features

-   **Fast Image Uploads**: Accepts common image formats via a web form, JSON `base64` string, or remote URL.
-   **Automatic Optimization**: Converts images to modern, efficient formats like WebP and optimizes PNGs.
-   **Background Processing**: Heavy optimization tasks are run in the background to ensure fast API responses.
-   **Thumbnail Generation**: Automatically creates small thumbnails for previews.
-   **Easy Deployment**: Fully containerized with Docker for simple setup and scaling.
-   **Imgur-like JSON API**: Provides a detailed, well-structured API for uploading and retrieving image data.

## Tech Stack

-   **Backend**: Rust, Rocket.rs
-   **Database**: MongoDB
-   **Image Processing**: `image`, `webp`, `oxipng` crates
-   **Deployment**: Docker, Docker Compose

## Getting Started

### Prerequisites

-   [Docker](https://docs.docker.com/get-docker/)
-   [Docker Compose](https://docs.docker.com/compose/install/)

### Installation & Setup

1.  **Clone the repository:**
    ```bash
    git clone <your-repository-url>
    cd image-host-api
    ```

2.  **Configure Environment Variables:**
    The application is configured using a `docker-compose.yml` file. The provided configuration is ready to run out of the box for local development.

    ```yaml
    # docker-compose.yml
    version: "3.8"

    services:
      app:
        build: .
        ports:
          - "8000:8080"
        environment:
          - MONGODB_URI=mongodb://mongo:27017
          - MONGODB_DB_NAME=image_host
          - HOST=localhost:8000 # Important for generating correct response URLs
          - ROCKET_ADDRESS=0.0.0.0
          - ROCKET_PORT=8080
        depends_on:
          - mongo

      mongo:
        image: mongo:latest
        restart: always
        volumes:
          - mongodb_data:/data/db

    volumes:
      mongodb_data:
    ```    -   **`HOST`**: This is crucial. It tells the application what hostname to use when generating full URLs in API responses. For production, you would change `localhost:8000` to your public domain name (e.g., `i.yourdomain.com`).

3.  **Build and Run the Application:**
    Use Docker Compose to build the images and start the services in the background.

    ```bash
    docker compose up --build -d
    ```

4.  **Access the Service:**
    Your image hosting service is now running!
    -   **Web Interface**: `http://localhost:8000`
    -   **API Base URL**: `http://localhost:8000`

## API Endpoints

### User Interface

---

#### `GET /`

-   **Description**: Serves the main HTML page with the image upload form.
-   **Response**: `200 OK` with `Content-Type: text/html`.

#### `POST /`

-   **Description**: Handles image uploads from the web interface. Expects a `multipart/form-data` payload with a file field named `image`.
-   **Response**: `303 See Other` - Redirects the user to the view page for the newly uploaded image (e.g., `/i/<image_id>`).

### JSON API

---

#### `POST /api/upload`

-   **Description**: The primary API endpoint for uploading an image. It supports three content types: `multipart/form-data`, and `application/json` (with either a `base64` string or a remote `url`).

-   **1. Multipart Form Data**
    -   **Content-Type**: `multipart/form-data`
    -   **Body**: Must contain a file field named `image`.
    -   **Example (`curl`)**:
        ```bash
        curl -F "image=@/path/to/your/image.jpg" http://localhost:8000/api/upload
        ```

-   **2. JSON with Base64**
    -   **Content-Type**: `application/json`
    -   **Body**: A JSON object with a single key `base64` containing the base64-encoded image string.
    -   **Example (`curl`)**:
        ```bash
        curl -H "Content-Type: application/json" \
             -d '{ "base64": "iVBORw0KGgoAAAANSUhEUgAAAAUAAAAFCAYAAACNbyblAAAAHElEQVQI12P4//8/w38GIAXDIBKE0DHxgljNBAAO9TXL0Y4OHwAAAABJRU5ErkJggg==" }' \
             http://localhost:8000/api/upload
        ```

-   **3. JSON with URL**
    -   **Content-Type**: `application/json`
    -   **Body**: A JSON object with a single key `url` pointing to a publicly accessible image.
    -   **Example (`curl`)**:
        ```bash
        curl -H "Content-Type: application/json" \
             -d '{ "url": "https://www.rust-lang.org/static/images/rust-logo-512x512.png" }' \
             http://localhost:8000/api/upload
        ```

-   **Success Response (`200 OK`)**:
    -   **Content-Type**: `application/json`
    -   **Body**: A detailed JSON object containing URLs, dimensions, and other metadata for the uploaded image.

    **Example Response Body:**
    ```json
    {
      "data": {
        "id": "pQrst7wXyZ",
        "title": "pQrst7wXyZ",
        "url_viewer": "https://localhost:8000/i/pQrst7wXyZ",
        "url": "https://localhost:8000/i/pQrst7wXyZ",
        "display_url": "https://localhost:8000/i/pQrst7wXyZ",
        "width": "1024",
        "height": "768",
        "size": "56288",
        "time": "1758134400",
        "expiration": "0",
        "image": {
          "filename": "pQrst7wXyZ.webp",
          "name": "pQrst7wXyZ",
          "mime": "image/webp",
          "extension": "webp",
          "url": "https://localhost:8000/i/pQrst7wXyZ"
        },
        "thumb": {
          "filename": "pQrst7wXyZ.webp",
          "name": "pQrst7wXyZ",
          "mime": "image/webp",
          "extension": "webp",
          "url": "https://localhost:8000/i/pQrst7wXyZ/thumb"
        },
        "medium": {
            "filename": "pQrst7wXyZ.webp",
            "name": "pQrst7wXyZ",
            "mime": "image/webp",
            "extension": "webp",
            "url": "https://localhost:8000/i/pQrst7wXyZ"
        },
        "delete_url": "https://localhost:8000/i/pQrst7wXyZ/delete/placeholder"
      },
      "success": true,
      "status": 200
    }
    ```

### Image Viewing

---

#### `GET /i/<id>`

-   **Description**: Retrieves and displays the raw image data for the specified ID. The `Content-Type` header of the response will match the optimized format of the stored image (e.g., `image/webp`).
-   **Parameters**:
    -   `id` (string): The unique ID of the image.
-   **Response**: `200 OK` with binary image data or `404 Not Found`.

#### `GET /i/<id>/thumb`

-   **Description**: Retrieves the raw thumbnail data for the specified ID.
-   **Parameters**:
    -   `id` (string): The unique ID of the image.
-   **Response**: `200 OK` with binary thumbnail data or `404 Not Found`.

#### `GET /image/<id>`

-   **Description**: A legacy endpoint for compatibility. It permanently redirects to the `/i/<id>` endpoint.
-   **Response**: `308 Permanent Redirect` to `/i/<id>`.

## License

This project is licensed under the MIT License. See the `LICENSE` file for details.