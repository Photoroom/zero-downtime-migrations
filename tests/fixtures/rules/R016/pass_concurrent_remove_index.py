# R016: RemoveIndexConcurrently - should pass
from django.db import migrations
from django.contrib.postgres.operations import RemoveIndexConcurrently


class Migration(migrations.Migration):

    atomic = False

    dependencies = [
        ('myapp', '0011'),
    ]

    operations = [
        RemoveIndexConcurrently(
            model_name='order',
            name='order_created_idx',
        ),
    ]
